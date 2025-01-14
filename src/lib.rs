extern crate url;

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::Arc;
use std::thread;
use std::net::SocketAddr;
use std::io::Error as IoError;
use std::path::Path;
use std::os::raw::c_char;
use std::ffi::CStr;
use log::{error, warn, info};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::{connect_async, accept_async};
use futures::executor::block_on;
use futures_util::{SinkExt, StreamExt, future};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response};
use hyper_staticfile::Static;

extern crate nanoid;

#[repr(C)]
#[derive(Copy, Clone)]
pub enum SocketResult {
    INVALIDHANDLE,
    CLOSED,
    OPENING,
    NODATA,
    HASDATA,
    ERROR,
    NEWCLIENT,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub enum SocketStatus {
    INVALIDHANDLE,
    CLOSED,
    OPEN,
    OPENING,
    ERROR,
}


struct ClientConn {
    tx: tokio::sync::mpsc::Sender<SocketMessage>, 
    rx: std::sync::mpsc::Receiver<SocketMessage>, 
    id: String,
}


enum SocketMessage {
    Connect,
    Disconnect,
    Message(String),
    BinaryMessage(Vec<u8>),
    Error(String),
    NewClient(ClientConn),
    FileAdd(String, Vec<u8>),
    FileRemove(String),
}


pub struct PollnetSocket {
    status: SocketStatus,
    tx: tokio::sync::mpsc::Sender<SocketMessage>,
    rx: std::sync::mpsc::Receiver<SocketMessage>,
    message: Option<Vec<u8>>,
    error: Option<String>,
    last_client_handle: u32,
}

pub struct PollnetContext {
    sockets: HashMap<u32, Box<PollnetSocket>>,
    next_handle: u32,
    thread: Option<thread::JoinHandle<()>>,
    rt_handle: tokio::runtime::Handle,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<i32>>,
}

#[derive(Debug)]
enum RecvError {
    Empty,
    Disconnected,
}

async fn accept_ws(tcp_stream: TcpStream, addr: SocketAddr, outer_tx: std::sync::mpsc::Sender<SocketMessage>) {//rx_to_sock: tokio::sync::mpsc::Receiver<SocketMessage>, tx_from_sock: std::sync::mpsc::Sender<SocketMessage>) {
    let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
    let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

    outer_tx.send(SocketMessage::NewClient(ClientConn{
        tx: tx_to_sock,
        rx: rx_from_sock,
        id: addr.to_string(), //"BLURGH".to_string(),
    })).expect("this shouldn't ever break?");

    match accept_async(tcp_stream).await {
        Ok(mut ws_stream) => {
            tx_from_sock.send(SocketMessage::Connect).expect("oh boy");
            loop {
                tokio::select! {
                    from_c_message = rx_to_sock.recv() => {
                        match from_c_message {
                            Some(SocketMessage::Message(msg)) => {
                                ws_stream.send(tungstenite::protocol::Message::Text(msg)).await.expect("WS send error");
                            },
                            Some(SocketMessage::BinaryMessage(msg)) => {
                                ws_stream.send(tungstenite::protocol::Message::Binary(msg)).await.expect("WS send error");
                            },
                            _ => break
                        }
                    },
                    from_sock_message = ws_stream.next() => {
                        match from_sock_message {
                            Some(Ok(msg)) => {
                                tx_from_sock.send(SocketMessage::BinaryMessage(msg.into_data())).expect("TX error on socket message");
                            },
                            Some(Err(msg)) => {
                                tx_from_sock.send(SocketMessage::Error(msg.to_string())).expect("TX error on socket error");
                                break;
                            },
                            None => {
                                tx_from_sock.send(SocketMessage::Disconnect).expect("TX error on disconnect");
                                break;
                            }
                        }
                    },
                };
            }
        },
        Err(err) => {
            error!("connection error: {}", err);
            tx_from_sock.send(SocketMessage::Error(err.to_string())).expect("TX error on connection error");
        }
    }
}

async fn accept_tcp(mut tcp_stream: TcpStream, addr: SocketAddr, outer_tx: Option<std::sync::mpsc::Sender<SocketMessage>>) {
    let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
    let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

    if let Some(tx) = outer_tx {
        tx.send(SocketMessage::NewClient(ClientConn{
            tx: tx_to_sock,
            rx: rx_from_sock,
            id: addr.to_string(),
        })).expect("this shouldn't ever break?");
    }

    tx_from_sock.send(SocketMessage::Connect).expect("oh boy");
    let mut buf = [0; 65536];
    loop {
        tokio::select! {
            from_c_message = rx_to_sock.recv() => {
                match from_c_message {
                    Some(SocketMessage::Message(msg)) => {
                        tcp_stream.write_all(msg.as_bytes()).await.expect("TCP send error");
                    },
                    Some(SocketMessage::BinaryMessage(msg)) => {
                        tcp_stream.write_all(&msg).await.expect("TCP send error");
                    },
                    _ => break
                }
            },
            _ = tcp_stream.readable() => {
                match tcp_stream.try_read(&mut buf){
                    Ok(n) => {
                        // TODO: can we avoid these copies? Does it matter?
                        let submessage = buf[0..n].to_vec();
                        tx_from_sock.send(SocketMessage::BinaryMessage(submessage)).expect("TX error on socket message");
                    }
                    Err(ref e) if e.kind() == tokio::io::ErrorKind::WouldBlock => {
                        // no effect?
                    }
                    Err(err) => {
                        tx_from_sock.send(SocketMessage::Error(err.to_string())).expect("TX error on socket error");
                        break;
                    }
                }
            },
        };
    }
    info!("Closing TCP socket!");
    tcp_stream.shutdown().await.unwrap_or_default(); // if this errors we don't care
}

async fn handle_http_request<B>(req: Request<B>, static_: Option<Static>, virtual_files: Arc<RwLock<HashMap<String, Vec<u8>>>>) -> Result<Response<Body>, IoError> {
    {
        // Do we need like... more headers???
        let vfiles = virtual_files.read().expect("RwLock poisoned");
        if let Some(file_data) = vfiles.get(req.uri().path()) {
            return Response::builder()
                    .status(http::StatusCode::OK)
                    .body(Body::from(file_data.clone()))
                    .map_err(|_| IoError::new(std::io::ErrorKind::Other, "Rust errors are a pain"))
        }
    }

    match static_ {
        Some(static_) => static_.clone().serve(req).await,
        None => {
            Response::builder().status(http::StatusCode::NOT_FOUND).body(Body::empty()).map_err(|_| IoError::new(std::io::ErrorKind::Other, "Rust errors are a pain"))
        }
    }
}

impl PollnetContext {
    fn new() -> PollnetContext {
        match env_logger::try_init() {
            Err(err) => warn!("Multiple contexts created!: {}", err),
            _ => (),
        }

        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let shutdown_tx = Some(shutdown_tx);

        let thread = Some(thread::spawn(move || {
            let rt = runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Unable to create the runtime");

            // Send handle back out so we can store it?
            handle_tx
                .send(rt.handle().clone())
                .expect("Unable to give runtime handle to another thread");

            // Continue running until notified to shutdown
            info!("tokio runtime starting");
            rt.block_on(async {
                shutdown_rx.await.unwrap();
                // uh let's just put in a 'safety' delay to shut everything down?
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            });
            rt.shutdown_timeout(std::time::Duration::from_millis(200));
            info!("tokio runtime shutdown");
        }));

        PollnetContext{
            next_handle: 1,
            rt_handle: handle_rx.recv().unwrap(),
            thread: thread,
            shutdown_tx: shutdown_tx,
            sockets: HashMap::new()
        }
    }

    fn _next_handle_that_satisfies_the_borrow_checker(next_handle: &mut u32) -> u32 {
        let new_handle: u32 = *next_handle;
        *next_handle += 1;
        new_handle   
    }

    fn _next_handle(&mut self) -> u32 {
        PollnetContext::_next_handle_that_satisfies_the_borrow_checker(&mut self.next_handle)
    }

    fn serve_http(&mut self, bind_addr: String, serve_dir: Option<String>) -> u32 {
        let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
        let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

        // Spawn a future onto the runtime
        self.rt_handle.spawn(async move {
            info!("HTTP server spawned");
            let addr = bind_addr.parse();
            if let Err(_) = addr {
                error!("Invalid TCP address: {}", bind_addr);
                tx_from_sock.send(SocketMessage::Error("Invalid TCP address".to_string())).unwrap_or_default();
                return;
            }
            let addr = addr.unwrap();

            let static_ = match serve_dir {
                Some(path_string) => Some(Static::new(Path::new(&path_string))),
                None => None
            };

            let virtual_files: HashMap<String, Vec<u8>> = HashMap::new();
            let virtual_files = Arc::new(RwLock::new(virtual_files));
            let virtual_files_two_the_clone_wars = virtual_files.clone();

            let make_service = make_service_fn(|_| {
                // Rust demands all these clones for reasons I don't fully understand
                // I definitely feel so much safer though!
                let static_ = static_.clone();
                let virtual_files = virtual_files.clone();
                future::ok::<_, hyper::Error>(service_fn(move |req| handle_http_request(req, static_.clone(), virtual_files.clone())))
            });

            let server = hyper::Server::try_bind(&addr);
            if let Err(bind_err) = server {
                error!("Couldn't bind {}: {}", bind_addr, bind_err);
                tx_from_sock.send(SocketMessage::Error(bind_err.to_string())).unwrap_or_default();
                return;
            }
            let server = server.unwrap().serve(make_service);
            let graceful = server.with_graceful_shutdown(async move {
                let virtual_files = virtual_files_two_the_clone_wars.clone();
                loop {
                    match rx_to_sock.recv().await {
                        Some(SocketMessage::Disconnect) | Some(SocketMessage::Error(_)) | None => {
                            break
                        },
                        Some(SocketMessage::FileAdd(filename, filedata)) => {
                            let mut vfiles = virtual_files.write().expect("Lock is poisoned");
                            vfiles.insert(filename, filedata);
                        },
                        Some(SocketMessage::FileRemove(filename)) => {
                            let mut vfiles = virtual_files.write().expect("Lock is poisoned");
                            vfiles.remove(&filename);
                        },
                        _ => {} // ignore sends?
                    }
                }
                info!("HTTP server trying to gracefully exit?");
            });
            info!("HTTP server running on http://{}/", addr);
            if let Err(err) = graceful.await {
                tx_from_sock.send(SocketMessage::Error(err.to_string())).unwrap_or_default(); // don't care at this point
            }
            info!("HTTP server stopped.");
        });

        let socket = Box::new(PollnetSocket{
            tx: tx_to_sock,
            rx: rx_from_sock,
            status: SocketStatus::OPENING,
            message: None,
            error: None,
            last_client_handle: 0
        });
        let new_handle = self._next_handle();
        self.sockets.insert(new_handle, socket);

        new_handle
    }

    fn listen_ws(&mut self, addr: String) -> u32 {
        let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
        let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

        self.rt_handle.spawn(async move {
            info!("WS server spawned");
            let listener = match TcpListener::bind(&addr).await {
                Ok(listener) => listener,
                Err(tcp_err) => {
                    tx_from_sock.send(SocketMessage::Error(tcp_err.to_string())).unwrap_or_default();
                    return;
                }
            };
            info!("WS server waiting for connections on {}", addr);
            tx_from_sock.send(SocketMessage::Connect).expect("oh boy");                    
            loop {
                tokio::select! {
                    from_c_message = rx_to_sock.recv() => {
                        match from_c_message {
                            Some(SocketMessage::Message(_msg)) => {}, // server socket ignores sends
                            _ => break
                        }
                    },
                    new_client = listener.accept() => {
                        match new_client {
                            Ok((tcp_stream, addr)) => {
                                tokio::spawn(accept_ws(tcp_stream, addr, tx_from_sock.clone()));
                            },
                            Err(msg) => {
                                tx_from_sock.send(SocketMessage::Error(msg.to_string())).expect("TX error on socket error");
                                break;
                            }
                        }
                    },
                };
            }
        });

        let socket = Box::new(PollnetSocket{
            tx: tx_to_sock,
            rx: rx_from_sock,
            status: SocketStatus::OPENING,
            message: None,
            error: None,
            last_client_handle: 0
        });
        let new_handle = self._next_handle();
        self.sockets.insert(new_handle, socket);

        new_handle
    }

    fn listen_tcp(&mut self, addr: String) -> u32 {
        let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
        let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

        self.rt_handle.spawn(async move {
            info!("TCP server spawned");
            let listener = match TcpListener::bind(&addr).await {
                Ok(listener) => listener,
                Err(tcp_err) => {
                    tx_from_sock.send(SocketMessage::Error(tcp_err.to_string())).unwrap_or_default();
                    return;
                }
            };
            info!("TCP server waiting for connections on {}", addr);
            tx_from_sock.send(SocketMessage::Connect).expect("oh boy");                    
            loop {
                tokio::select! {
                    from_c_message = rx_to_sock.recv() => {
                        match from_c_message {
                            Some(SocketMessage::Message(_msg)) => {}, // server socket ignores sends
                            _ => break
                        }
                    },
                    new_client = listener.accept() => {
                        match new_client {
                            Ok((tcp_stream, addr)) => {
                                tokio::spawn(accept_tcp(tcp_stream, addr, Some(tx_from_sock.clone())));
                            },
                            Err(msg) => {
                                tx_from_sock.send(SocketMessage::Error(msg.to_string())).expect("TX error on socket error");
                                break;
                            }
                        }
                    },
                };
            }
        });

        let socket = Box::new(PollnetSocket{
            tx: tx_to_sock,
            rx: rx_from_sock,
            status: SocketStatus::OPENING,
            message: None,
            error: None,
            last_client_handle: 0
        });
        let new_handle = self._next_handle();
        self.sockets.insert(new_handle, socket);

        new_handle
    }

    fn open_ws(&mut self, url: String) -> u32 {
        let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
        let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

        self.rt_handle.spawn(async move {
            info!("WS client spawned");
            let real_url = url::Url::parse(&url);
            if let Err(url_err) = real_url {
                error!("Invalid URL: {}", url);
                tx_from_sock.send(SocketMessage::Error(url_err.to_string())).unwrap_or_default();
                return;
            }

            info!("WS client attempting to connect to {}", url);
            match connect_async(real_url.unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    tx_from_sock.send(SocketMessage::Connect).expect("oh boy");
                    loop {
                        tokio::select! {
                            from_c_message = rx_to_sock.recv() => {
                                match from_c_message {
                                    Some(SocketMessage::Message(msg)) => {
                                        ws_stream.send(tungstenite::protocol::Message::Text(msg)).await.expect("WS send error");
                                    },
                                    Some(SocketMessage::BinaryMessage(msg)) => {
                                        ws_stream.send(tungstenite::protocol::Message::Binary(msg)).await.expect("WS send error");
                                    },
                                    _ => break
                                }
                            },
                            from_sock_message = ws_stream.next() => {
                                match from_sock_message {
                                    Some(Ok(msg)) => {
                                        tx_from_sock.send(SocketMessage::BinaryMessage(msg.into_data())).expect("TX error on socket message");
                                    },
                                    Some(Err(msg)) => {
                                        tx_from_sock.send(SocketMessage::Error(msg.to_string())).expect("TX error on socket error");
                                        break;
                                    },
                                    None => {
                                        tx_from_sock.send(SocketMessage::Disconnect).expect("TX error on remote socket close");
                                        break;
                                    }
                                }
                            },
                        };
                    }
                    info!("Closing websocket!");
                    ws_stream.close(None).await.unwrap_or_default(); // if this errors we don't care
                },
                Err(err) => {
                    error!("WS client connection error: {}", err);
                    tx_from_sock.send(SocketMessage::Error(err.to_string())).expect("TX error on connection error");
                }
            }
        });

        let socket = Box::new(PollnetSocket{
            tx: tx_to_sock,
            rx: rx_from_sock,
            status: SocketStatus::OPENING,
            message: None,
            error: None,
            last_client_handle: 0
        });
        let new_handle = self._next_handle();
        self.sockets.insert(new_handle, socket);

        new_handle
    }

    fn open_tcp(&mut self, addr: String) -> u32 {
        let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
        let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

        self.rt_handle.spawn(async move {
            info!("TCP client attempting to connect to {}", addr);
            let mut buf = [0; 65536];
            match TcpStream::connect(addr).await {
                Ok(mut tcp_stream) => {
                    tx_from_sock.send(SocketMessage::Connect).expect("oh boy");
                    loop {
                        tokio::select! {
                            from_c_message = rx_to_sock.recv() => {
                                match from_c_message {
                                    Some(SocketMessage::Message(msg)) => {
                                        tcp_stream.write_all(msg.as_bytes()).await.expect("TCP send error");
                                    },
                                    Some(SocketMessage::BinaryMessage(msg)) => {
                                        tcp_stream.write_all(&msg).await.expect("TCP send error");
                                    },
                                    _ => break
                                }
                            },
                            _ = tcp_stream.readable() => {
                                match tcp_stream.try_read(&mut buf){
                                    Ok(n) => {
                                        // TODO: can we avoid these copies? Does it matter?
                                        let submessage = buf[0..n].to_vec();
                                        tx_from_sock.send(SocketMessage::BinaryMessage(submessage)).expect("TX error on socket message");
                                    }
                                    Err(ref e) if e.kind() == tokio::io::ErrorKind::WouldBlock => {
                                        // no effect?
                                    }
                                    Err(err) => {
                                        tx_from_sock.send(SocketMessage::Error(err.to_string())).expect("TX error on socket error");
                                        break;
                                    }
                                }
                            },
                        };
                    }
                    info!("Closing TCP socket!");
                    tcp_stream.shutdown().await.unwrap_or_default(); // if this errors we don't care
                },
                Err(err) => {
                    error!("TCP client connection error: {}", err);
                    tx_from_sock.send(SocketMessage::Error(err.to_string())).expect("TX error on connection error");
                }
            }
        });

        let socket = Box::new(PollnetSocket{
            tx: tx_to_sock,
            rx: rx_from_sock,
            status: SocketStatus::OPENING,
            message: None,
            error: None,
            last_client_handle: 0
        });
        let new_handle = self._next_handle();
        self.sockets.insert(new_handle, socket);

        new_handle
    }

    async fn _handle_get(url: String, dest: std::sync::mpsc::Sender<SocketMessage>) {
        info!("HTTP GET: {}", url);
        let resp = match reqwest::get(&url).await {
            Ok(resp) => resp,
            Err(err) => {
                error!("HTTP GET failed: {}", err);
                dest.send(SocketMessage::Error(err.to_string())).expect("TX error on http post error");
                return;
            }
        };
        match resp.bytes().await {
            Ok(body) => {
                dest.send(SocketMessage::BinaryMessage(body.to_vec())).expect("TX error on http body");
            },
            Err(body_err) => {
                dest.send(SocketMessage::Error(body_err.to_string())).expect("TX error on http body error");
            }
        };
    }

    fn open_http_get_simple(&mut self, url: String) -> u32 {
        let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
        let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

        self.rt_handle.spawn(async move {
            let get_handler = PollnetContext::_handle_get(url, tx_from_sock);
            tokio::pin!(get_handler);
            loop {
                tokio::select! {
                    _ = &mut get_handler => break,
                    from_c_message = rx_to_sock.recv() => {
                        match from_c_message {
                            Some(SocketMessage::Disconnect) => break,
                            _ => ()
                        }
                    },
                }
            }
        });

        let socket = Box::new(PollnetSocket{
            tx: tx_to_sock,
            rx: rx_from_sock,
            status: SocketStatus::OPENING,
            message: None,
            error: None,
            last_client_handle: 0
        });
        let new_handle = self._next_handle();
        self.sockets.insert(new_handle, socket);

        new_handle
    }

    async fn _handle_post(url: String, content_type: String, body: Vec<u8>, dest: std::sync::mpsc::Sender<SocketMessage>) {
        info!("HTTP POST: {} (w/ {})", url, content_type);
        let client = reqwest::Client::new();
        let resp = match client.post(&url).header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body).send().await {
            Ok(resp) => resp,
            Err(err) => {
                error!("HTTP POST failed: {}", err);
                dest.send(SocketMessage::Error(err.to_string())).expect("TX error on http post error");
                return;
            }
        };
        match resp.bytes().await {
            Ok(body) => {
                dest.send(SocketMessage::BinaryMessage(body.to_vec())).expect("TX error on http body");
            },
            Err(body_err) => {
                dest.send(SocketMessage::Error(body_err.to_string())).expect("TX error on http body error");
            }
        };
    }

    fn open_http_post_simple(&mut self, url: String, content_type: String, body: Vec<u8>) -> u32 {
        let (tx_to_sock, mut rx_to_sock) = tokio::sync::mpsc::channel(100);
        let (tx_from_sock, rx_from_sock) = std::sync::mpsc::channel();

        self.rt_handle.spawn(async move {
            let post_handler = PollnetContext::_handle_post(url, content_type, body, tx_from_sock);
            tokio::pin!(post_handler);
            loop {
                tokio::select! {
                    _ = &mut post_handler => break,
                    from_c_message = rx_to_sock.recv() => {
                        match from_c_message {
                            Some(SocketMessage::Disconnect) => break,
                            _ => ()
                        }
                    },
                }
            }
        });

        let socket = Box::new(PollnetSocket{
            tx: tx_to_sock,
            rx: rx_from_sock,
            status: SocketStatus::OPENING,
            message: None,
            error: None,
            last_client_handle: 0
        });
        let new_handle = self._next_handle();
        self.sockets.insert(new_handle, socket);

        new_handle
    }

    fn close_all(&mut self) {
        info!("Closing all sockets!");
        for (_, sock) in self.sockets.iter_mut() {
            match sock.status {
                SocketStatus::OPEN | SocketStatus::OPENING => {
                    // don't care about errors at this point
                    block_on(sock.tx.send(SocketMessage::Disconnect)).unwrap_or_default();
                    sock.status = SocketStatus::CLOSED;
                },
                _ => (),
            }
        }
        self.sockets.clear(); // everything should be closed and safely droppable
    }

    fn close(&mut self, handle: u32) {
        if let Some(sock) = self.sockets.get_mut(&handle) {
            match sock.status {
                SocketStatus::OPEN | SocketStatus::OPENING => {
                    match block_on(sock.tx.send(SocketMessage::Disconnect)) {
                        _ => ()
                    }
                    sock.status = SocketStatus::CLOSED;
                },
                _ => (),
            }
            // Note: since we don't wait here for any kind of "disconnect" reply,
            // a socket that has been closed should just return without sending a reply
            self.sockets.remove(&handle);
        }
    }

    fn send(&mut self, handle: u32, msg: String) {
        if let Some(sock) = self.sockets.get_mut(&handle) {
            match sock.status {
                SocketStatus::OPEN | SocketStatus::OPENING => {
                    sock.tx.try_send(SocketMessage::Message(msg)).unwrap_or_default()
                },
                _ => (),
            };
        }
    }

    fn send_binary(&mut self, handle: u32, msg: Vec<u8>) {
        if let Some(sock) = self.sockets.get_mut(&handle) {
            match sock.status {
                SocketStatus::OPEN | SocketStatus::OPENING => {
                    sock.tx.try_send(SocketMessage::BinaryMessage(msg)).unwrap_or_default()
                },
                _ => (),
            };
        }
    }

    fn add_virtual_file(&mut self, handle: u32, filename: String, filedata: Vec<u8>) {
        if let Some(sock) = self.sockets.get_mut(&handle) {
            match sock.status {
                SocketStatus::OPEN | SocketStatus::OPENING => {
                    sock.tx.try_send(SocketMessage::FileAdd(filename, filedata)).unwrap_or_default()
                },
                _ => (),
            };
        }
    }

    fn remove_virtual_file(&mut self, handle: u32, filename: String) {
        if let Some(sock) = self.sockets.get_mut(&handle) {
            match sock.status {
                SocketStatus::OPEN | SocketStatus::OPENING => {
                    sock.tx.try_send(SocketMessage::FileRemove(filename)).unwrap_or_default()
                },
                _ => (),
            };
        }
    }

    fn update(&mut self, handle: u32, blocking: bool) -> SocketResult {
        let sock = match self.sockets.get_mut(&handle) {
            Some(sock) => sock,
            None => return SocketResult::INVALIDHANDLE,
        };

        match sock.status {
            SocketStatus::OPEN | SocketStatus::OPENING => {
                // This block is apparently impossible to move into a helper function
                // for borrow checker "reasons"
                let result = if blocking {
                    sock.rx.recv().map_err(|_err| RecvError::Disconnected)
                } else {
                    sock.rx.try_recv().map_err(|err| match err {
                        std::sync::mpsc::TryRecvError::Empty => RecvError::Empty,
                        std::sync::mpsc::TryRecvError::Disconnected => RecvError::Disconnected,
                    })
                };

                match result {
                    Ok(SocketMessage::Connect) => {
                        sock.status = SocketStatus::OPEN;
                        SocketResult::OPENING
                    },
                    Ok(SocketMessage::Disconnect) | Err(RecvError::Disconnected) => {
                        sock.status = SocketStatus::CLOSED;
                        SocketResult::CLOSED
                    },
                    Ok(SocketMessage::Message(msg)) => {
                        sock.message = Some(msg.into_bytes());
                        SocketResult::HASDATA
                    },
                    Ok(SocketMessage::BinaryMessage(msg)) => {
                        sock.message = Some(msg);
                        SocketResult::HASDATA
                    },
                    Ok(SocketMessage::Error(err)) => {
                        sock.error = Some(err);
                        sock.status = SocketStatus::ERROR;
                        SocketResult::ERROR
                    },
                    Ok(SocketMessage::NewClient(conn)) => {
                        // can't use self._next_handle() either for questionable reasons
                        let new_handle = PollnetContext::_next_handle_that_satisfies_the_borrow_checker(&mut self.next_handle);
                        sock.last_client_handle = new_handle;
                        sock.message = Some(conn.id.into_bytes());
                        let client_socket = Box::new(PollnetSocket{
                            tx: conn.tx,
                            rx: conn.rx,
                            status: SocketStatus::OPEN, // assume client sockets start open?
                            message: None,
                            error: None,
                            last_client_handle: 0,
                        });
                        self.sockets.insert(new_handle, client_socket);
                        SocketResult::NEWCLIENT
                    },
                    Ok(_) => SocketResult::NODATA,
                    Err(RecvError::Empty) => SocketResult::NODATA,
                }
            },
            SocketStatus::CLOSED => SocketResult::CLOSED,
            _ => SocketResult::ERROR
        }
    }


    fn shutdown(&mut self) {
        info!("Starting shutdown");
        self.close_all();
        info!("All sockets should be closed?");
        if let Some(tx) = self.shutdown_tx.take() {
            tx.send(0).unwrap_or_default();
        }
        if let Some(handle) = self.thread.take() {
            handle.join().unwrap_or_default();
        }
        info!("Thread should be joined?");
    }
}

fn c_str_to_string(s: *const c_char) -> String {
    unsafe { CStr::from_ptr(s).to_string_lossy().into_owned() }
}

fn c_data_to_vec(data: *const u8, datasize: u32) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(data, datasize as usize).to_vec() }
}

#[no_mangle]
pub extern fn pollnet_init() -> *mut PollnetContext {
    Box::into_raw(Box::new(PollnetContext::new()))
}

#[no_mangle]
pub extern fn pollnet_shutdown(ctx: *mut PollnetContext) {
    info!("Requested ctx close!");
    let ctx = unsafe{&mut *ctx};
    ctx.shutdown();

    // take ownership and drop
    let b = unsafe{ Box::from_raw(ctx) };
    drop(b);
    info!("Everything should be dead now!");
}

#[no_mangle]
pub extern fn pollnet_open_ws(ctx: *mut PollnetContext, url: *const c_char) -> u32 {
    let ctx = unsafe{&mut *ctx};
    let url = c_str_to_string(url);
    ctx.open_ws(url)
}

#[no_mangle]
pub extern fn pollnet_listen_ws(ctx: *mut PollnetContext, addr: *const c_char) -> u32 {
    let ctx = unsafe{&mut *ctx};
    let addr = c_str_to_string(addr);
    ctx.listen_ws(addr)
}

#[no_mangle]
pub extern fn pollnet_open_tcp(ctx: *mut PollnetContext, addr: *const c_char) -> u32 {
    let ctx = unsafe{&mut *ctx};
    let addr = c_str_to_string(addr);
    ctx.open_tcp(addr)
}

#[no_mangle]
pub extern fn pollnet_listen_tcp(ctx: *mut PollnetContext, addr: *const c_char) -> u32 {
    let ctx = unsafe{&mut *ctx};
    let addr = c_str_to_string(addr);
    ctx.listen_tcp(addr)
}

#[no_mangle]
pub extern fn pollnet_simple_http_get(ctx: *mut PollnetContext, addr: *const c_char) -> u32 {
    let ctx = unsafe{&mut *ctx};
    let addr = c_str_to_string(addr);
    ctx.open_http_get_simple(addr)
}

#[no_mangle]
pub extern fn pollnet_simple_http_post(ctx: *mut PollnetContext, addr: *const c_char, content_type: *const c_char, bodydata: *const u8, bodysize: u32) -> u32 {
    let ctx = unsafe{&mut *ctx};
    let addr = c_str_to_string(addr);
    let content_type = c_str_to_string(content_type);
    let body = c_data_to_vec(bodydata, bodysize);
    ctx.open_http_post_simple(addr, content_type, body)
}

#[no_mangle]
pub extern fn pollnet_serve_static_http(ctx: *mut PollnetContext, addr: *const c_char, serve_dir: *const c_char) -> u32 {
    let ctx = unsafe{&mut *ctx};
    let addr = c_str_to_string(addr);
    let serve_dir = c_str_to_string(serve_dir);
    ctx.serve_http(addr, Some(serve_dir))
}

#[no_mangle]
pub extern fn pollnet_serve_http(ctx: *mut PollnetContext, addr: *const c_char) -> u32 {
    let ctx = unsafe{&mut *ctx};
    let addr = c_str_to_string(addr);
    ctx.serve_http(addr, None)
}

#[no_mangle]
pub extern fn pollnet_close(ctx: *mut PollnetContext, handle: u32) {
    let ctx = unsafe{&mut *ctx};
    ctx.close(handle)
}

#[no_mangle]
pub extern fn pollnet_close_all(ctx: *mut PollnetContext) {
    let ctx = unsafe{&mut *ctx};
    ctx.close_all()
}

#[no_mangle]
pub extern fn pollnet_status(ctx: *mut PollnetContext, handle: u32) -> SocketStatus {
    let ctx = unsafe{&*ctx};
    if let Some(socket) = ctx.sockets.get(&handle) {
        socket.status
    } else {
        SocketStatus::INVALIDHANDLE
    }
}

#[no_mangle]
pub extern fn pollnet_send(ctx: *mut PollnetContext, handle: u32, msg: *const c_char) {
    let ctx = unsafe{&mut *ctx};
    let msg = c_str_to_string(msg);
    ctx.send(handle, msg)
}

#[no_mangle]
pub extern fn pollnet_send_binary(ctx: *mut PollnetContext, handle: u32, msg: *const u8, msgsize: u32) {
    let ctx = unsafe{&mut *ctx};
    let msg = c_data_to_vec(msg, msgsize);
    ctx.send_binary(handle, msg)
}

#[no_mangle]
pub extern fn pollnet_add_virtual_file(ctx: *mut PollnetContext, handle: u32, filename: *const c_char, filedata: *const u8, datasize: u32) {
    let ctx = unsafe{&mut *ctx};
    let filename = c_str_to_string(filename);
    let filedata = c_data_to_vec(filedata, datasize);
    ctx.add_virtual_file(handle, filename, filedata)
}

#[no_mangle]
pub extern fn pollnet_remove_virtual_file(ctx: *mut PollnetContext, handle: u32, filename: *const c_char) {
    let ctx = unsafe{&mut *ctx};
    let filename = c_str_to_string(filename);
    ctx.remove_virtual_file(handle, filename)
}

#[no_mangle]
pub extern fn pollnet_update(ctx: *mut PollnetContext, handle: u32) -> SocketResult {
    let ctx = unsafe{&mut *ctx};
    ctx.update(handle, false)
}

#[no_mangle]
pub extern fn pollnet_update_blocking(ctx: *mut PollnetContext, handle: u32) -> SocketResult {
    let ctx = unsafe{&mut *ctx};
    ctx.update(handle, true)
}

#[no_mangle]
pub extern fn pollnet_get(ctx: *mut PollnetContext, handle: u32, dest: *mut u8, dest_size: u32) -> i32 {
    let ctx = unsafe{&mut *ctx};
    let socket = match ctx.sockets.get_mut(&handle) {
        Some(socket) => socket,
        None => return -1,
    };

    match socket.message.take() {
        Some(msg) => {
            let ncopy = msg.len();
            if ncopy < (dest_size as usize) {
                unsafe {
                    std::ptr::copy_nonoverlapping(msg.as_ptr(), dest, ncopy);
                }
                ncopy as i32
            } else {
                0
            }
        },
        None => 0,
    }
}

#[no_mangle]
pub extern fn pollnet_get_connected_client_handle(ctx: *mut PollnetContext, handle: u32) -> u32 {
    let ctx = unsafe{&mut *ctx};
    match ctx.sockets.get_mut(&handle) {
        Some(socket) => socket.last_client_handle,
        None => 0,
    }
}

#[no_mangle]
pub extern fn pollnet_get_error(ctx: *mut PollnetContext, handle: u32, dest: *mut u8, dest_size: u32) -> i32 {
    let ctx = unsafe{&mut *ctx};
    let socket = match ctx.sockets.get_mut(&handle) {
        Some(socket) => socket,
        None => return -1,
    };

    match socket.error.take() {
        Some(msg) => {
            let ncopy = msg.len();
            if ncopy < (dest_size as usize) {
                unsafe {
                    std::ptr::copy_nonoverlapping(msg.as_ptr(), dest, ncopy);
                }
                ncopy as i32
            } else {
                0
            }
        },
        None => 0,
    }
}

static mut HACKSTATICCONTEXT: *mut PollnetContext = 0 as *mut PollnetContext;

#[no_mangle]
pub unsafe extern fn pollnet_get_or_init_static() -> *mut PollnetContext {
    if HACKSTATICCONTEXT.is_null() {
        warn!("INITIALIZING HACK STATIC CONTEXT");
        HACKSTATICCONTEXT = Box::into_raw(Box::new(PollnetContext::new()))
    }
    HACKSTATICCONTEXT
}


#[no_mangle]
pub extern fn pollnet_get_nanoid(dest: *mut u8, dest_size: u32) -> i32 {
    let id = nanoid::nanoid!();
    if id.len() < (dest_size as usize) {
        unsafe {
            std::ptr::copy_nonoverlapping(id.as_ptr(), dest, id.len());
        }
        id.len() as i32
    } else {
        0
    }
}