use futures::{sink::SinkExt, StreamExt, TryStreamExt};
use http_body_util::BodyExt;
use hyper::body;
use pyo3::{prelude::*, pybacked::PyBackedStr};
use std::{
    borrow::Cow,
    sync::{atomic, Arc, Mutex, RwLock},
};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;

use super::{
    errors::{error_proto, error_stream},
    types::{PyResponse, PyResponseBody, PyResponseFile},
};
use crate::{
    conversion::FutureResultToPy,
    runtime::{future_into_py_futlike, Runtime, RuntimeRef},
    ws::{HyperWebsocket, UpgradeData, WSRxStream, WSStream, WSTxStream},
};

pub(crate) type WebsocketDetachedTransport = (i32, bool, Option<tokio::task::JoinHandle<()>>);

#[pyclass(frozen, module = "granian._granian")]
pub(crate) struct RSGIHTTPStreamTransport {
    rt: RuntimeRef,
    tx: mpsc::Sender<Result<body::Bytes, anyhow::Error>>,
}

impl RSGIHTTPStreamTransport {
    pub fn new(rt: RuntimeRef, transport: mpsc::Sender<Result<body::Bytes, anyhow::Error>>) -> Self {
        Self { rt, tx: transport }
    }
}

#[pymethods]
impl RSGIHTTPStreamTransport {
    fn send_bytes<'p>(&self, py: Python<'p>, data: Cow<[u8]>) -> PyResult<Bound<'p, PyAny>> {
        let transport = self.tx.clone();
        let bdata: Box<[u8]> = data.into();

        future_into_py_futlike(self.rt.clone(), py, async move {
            match transport.send(Ok(body::Bytes::from(bdata))).await {
                Ok(()) => FutureResultToPy::None,
                _ => FutureResultToPy::Err(error_stream!()),
            }
        })
    }

    fn send_str<'p>(&self, py: Python<'p>, data: String) -> PyResult<Bound<'p, PyAny>> {
        let transport = self.tx.clone();

        future_into_py_futlike(self.rt.clone(), py, async move {
            match transport.send(Ok(body::Bytes::from(data))).await {
                Ok(()) => FutureResultToPy::None,
                _ => FutureResultToPy::Err(error_stream!()),
            }
        })
    }
}

#[pyclass(frozen, module = "granian._granian")]
pub(crate) struct RSGIHTTPProtocol {
    rt: RuntimeRef,
    tx: Mutex<Option<oneshot::Sender<PyResponse>>>,
    body: Mutex<Option<body::Incoming>>,
    body_stream: Arc<AsyncMutex<Option<http_body_util::BodyStream<body::Incoming>>>>,
}

impl RSGIHTTPProtocol {
    pub fn new(rt: RuntimeRef, tx: oneshot::Sender<PyResponse>, body: body::Incoming) -> Self {
        Self {
            rt,
            tx: Mutex::new(Some(tx)),
            body: Mutex::new(Some(body)),
            body_stream: Arc::new(AsyncMutex::new(None)),
        }
    }

    pub fn tx(&self) -> Option<oneshot::Sender<PyResponse>> {
        self.tx.lock().unwrap().take()
    }
}

#[pymethods]
impl RSGIHTTPProtocol {
    fn __call__<'p>(&self, py: Python<'p>) -> PyResult<Bound<'p, PyAny>> {
        if let Some(body) = self.body.lock().unwrap().take() {
            return future_into_py_futlike(self.rt.clone(), py, async move {
                match body.collect().await {
                    Ok(data) => FutureResultToPy::Bytes(data.to_bytes()),
                    _ => FutureResultToPy::Err(error_stream!()),
                }
            });
        }
        error_proto!()
    }

    fn __aiter__(pyself: Py<Self>, py: Python) -> PyResult<Py<Self>> {
        let inner = pyself.get();
        if let Some(body) = inner.body.lock().unwrap().take() {
            let mut stream = inner.body_stream.blocking_lock();
            *stream = Some(http_body_util::BodyStream::new(body));
            return Ok(pyself.clone_ref(py));
        }
        error_proto!()
    }

    fn __anext__<'p>(&self, py: Python<'p>) -> PyResult<Bound<'p, PyAny>> {
        if self.body_stream.blocking_lock().is_none() {
            return Err(pyo3::exceptions::PyStopAsyncIteration::new_err("stream exhausted"));
        }
        let body_stream = self.body_stream.clone();
        future_into_py_futlike(self.rt.clone(), py, async move {
            let guard = &mut *body_stream.lock().await;
            match guard.as_mut().unwrap().next().await {
                Some(chunk) => {
                    let chunk = chunk
                        .map(|buf| buf.into_data().unwrap_or_default())
                        .unwrap_or(body::Bytes::new());
                    FutureResultToPy::Bytes(chunk)
                }
                _ => {
                    let _ = guard.take();
                    FutureResultToPy::Bytes(body::Bytes::new())
                }
            }
        })
    }

    #[pyo3(signature = (status=200, headers=vec![]))]
    fn response_empty(&self, status: u16, headers: Vec<(PyBackedStr, PyBackedStr)>) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(PyResponse::Body(PyResponseBody::empty(status, headers)));
        }
    }

    #[pyo3(signature = (status=200, headers=vec![], body=vec![].into()))]
    fn response_bytes(&self, status: u16, headers: Vec<(PyBackedStr, PyBackedStr)>, body: Cow<[u8]>) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(PyResponse::Body(PyResponseBody::from_bytes(status, headers, body)));
        }
    }

    #[pyo3(signature = (status=200, headers=vec![], body=String::new()))]
    fn response_str(&self, status: u16, headers: Vec<(PyBackedStr, PyBackedStr)>, body: String) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(PyResponse::Body(PyResponseBody::from_string(status, headers, body)));
        }
    }

    #[pyo3(signature = (status, headers, file))]
    fn response_file(&self, status: u16, headers: Vec<(PyBackedStr, PyBackedStr)>, file: String) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(PyResponse::File(PyResponseFile::new(status, headers, file)));
        }
    }

    #[pyo3(signature = (status=200, headers=vec![]))]
    fn response_stream<'p>(
        &self,
        py: Python<'p>,
        status: u16,
        headers: Vec<(PyBackedStr, PyBackedStr)>,
    ) -> PyResult<Bound<'p, RSGIHTTPStreamTransport>> {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let (body_tx, body_rx) = mpsc::channel::<Result<body::Bytes, anyhow::Error>>(1);
            let body_stream = http_body_util::StreamBody::new(
                tokio_stream::wrappers::ReceiverStream::new(body_rx).map_ok(body::Frame::data),
            );
            let _ = tx.send(PyResponse::Body(PyResponseBody::new(
                status,
                headers,
                BodyExt::boxed(BodyExt::map_err(body_stream, std::convert::Into::into)),
            )));
            let trx = Py::new(py, RSGIHTTPStreamTransport::new(self.rt.clone(), body_tx))?;
            return Ok(trx.into_bound(py));
        }
        error_proto!()
    }
}

#[pyclass(frozen, module = "granian._granian")]
pub(crate) struct RSGIWebsocketTransport {
    rt: RuntimeRef,
    tx: Arc<AsyncMutex<WSTxStream>>,
    rx: Arc<AsyncMutex<WSRxStream>>,
    closed: atomic::AtomicBool,
}

impl RSGIWebsocketTransport {
    pub fn new(rt: RuntimeRef, transport: WSStream) -> Self {
        let (tx, rx) = transport.split();
        Self {
            rt,
            tx: Arc::new(AsyncMutex::new(tx)),
            rx: Arc::new(AsyncMutex::new(rx)),
            closed: false.into(),
        }
    }

    pub fn close(&self) -> Option<tokio::task::JoinHandle<()>> {
        if self.closed.load(atomic::Ordering::Relaxed) {
            return None;
        }
        self.closed.store(true, atomic::Ordering::Relaxed);

        let tx = self.tx.clone();
        let handle = self.rt.spawn(async move {
            if let Ok(mut tx) = tx.try_lock() {
                if let Err(err) = tx.close().await {
                    log::info!("Failed to close websocket with error {:?}", err);
                }
            }
        });
        Some(handle)
    }
}

#[pymethods]
impl RSGIWebsocketTransport {
    fn receive<'p>(&self, py: Python<'p>) -> PyResult<Bound<'p, PyAny>> {
        let transport = self.rx.clone();
        future_into_py_futlike(self.rt.clone(), py, async move {
            if let Ok(mut stream) = transport.try_lock() {
                while let Some(recv) = stream.next().await {
                    match recv {
                        Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                        Ok(message) => return FutureResultToPy::RSGIWSMessage(message),
                        _ => break,
                    }
                }
                return FutureResultToPy::Err(error_stream!());
            }
            FutureResultToPy::Err(error_proto!())
        })
    }

    fn send_bytes<'p>(&self, py: Python<'p>, data: Cow<[u8]>) -> PyResult<Bound<'p, PyAny>> {
        let transport = self.tx.clone();
        let bdata: Box<[u8]> = data.into();

        future_into_py_futlike(self.rt.clone(), py, async move {
            if let Ok(mut stream) = transport.try_lock() {
                return match stream.send(bdata[..].into()).await {
                    Ok(()) => FutureResultToPy::None,
                    _ => FutureResultToPy::Err(error_stream!()),
                };
            }
            FutureResultToPy::Err(error_proto!())
        })
    }

    fn send_str<'p>(&self, py: Python<'p>, data: String) -> PyResult<Bound<'p, PyAny>> {
        let transport = self.tx.clone();

        future_into_py_futlike(self.rt.clone(), py, async move {
            if let Ok(mut stream) = transport.try_lock() {
                return match stream.send(data.into()).await {
                    Ok(()) => FutureResultToPy::None,
                    _ => FutureResultToPy::Err(error_stream!()),
                };
            }
            FutureResultToPy::Err(error_proto!())
        })
    }
}

#[pyclass(frozen, module = "granian._granian")]
pub(crate) struct RSGIWebsocketProtocol {
    rt: RuntimeRef,
    tx: Mutex<Option<oneshot::Sender<WebsocketDetachedTransport>>>,
    websocket: Arc<AsyncMutex<HyperWebsocket>>,
    upgrade: RwLock<Option<UpgradeData>>,
    transport: Arc<Mutex<Option<Py<RSGIWebsocketTransport>>>>,
}

impl RSGIWebsocketProtocol {
    pub fn new(
        rt: RuntimeRef,
        tx: oneshot::Sender<WebsocketDetachedTransport>,
        websocket: HyperWebsocket,
        upgrade: UpgradeData,
    ) -> Self {
        Self {
            rt,
            tx: Mutex::new(Some(tx)),
            websocket: Arc::new(AsyncMutex::new(websocket)),
            upgrade: RwLock::new(Some(upgrade)),
            transport: Arc::new(Mutex::new(None)),
        }
    }

    fn consumed(&self) -> bool {
        self.upgrade.read().unwrap().is_none()
    }
}

#[pymethods]
impl RSGIWebsocketProtocol {
    #[pyo3(signature = (status=None))]
    pub fn close(&self, status: Option<i32>) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let mut handle = None;
            if let Ok(mut transport) = self.transport.try_lock() {
                if let Some(transport) = transport.take() {
                    handle = transport.get().close();
                }
            }

            let _ = tx.send((status.unwrap_or(0), self.consumed(), handle));
        }
    }

    fn accept<'p>(&self, py: Python<'p>) -> PyResult<Bound<'p, PyAny>> {
        let rth = self.rt.clone();
        let mut upgrade = self.upgrade.write().unwrap().take().unwrap();
        let transport = self.websocket.clone();
        let itransport = self.transport.clone();
        future_into_py_futlike(self.rt.clone(), py, async move {
            let mut ws = transport.lock().await;
            match upgrade.send(None).await {
                Ok(()) => match (&mut *ws).await {
                    Ok(stream) => {
                        let mut trx = itransport.lock().unwrap();
                        Python::with_gil(|py| {
                            let pytransport = Py::new(py, RSGIWebsocketTransport::new(rth, stream)).unwrap();
                            *trx = Some(pytransport.clone_ref(py));
                            FutureResultToPy::Py(pytransport.into_any())
                        })
                    }
                    _ => FutureResultToPy::Err(error_proto!()),
                },
                _ => FutureResultToPy::Err(error_proto!()),
            }
        })
    }
}
