#![allow(clippy::single_match, dead_code)]

use std::{
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use context::AppContextRef;
use socket2::{Domain, Socket};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    global_shortcut::ShortcutManager,
    menu::{ContextMenu, MenuType},
    system_tray::SystemTrayBuilder,
    window::{Icon, WindowBuilder},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
};
use tokio_rustls::{
    rustls::{ClientConfig, ServerConfig, ServerName},
    TlsAcceptor, TlsConnector,
};

mod packet;
use packet::{IdentityPacket, NetworkPacket, NetworkPacketWithPayload};

mod cache;
mod config;
mod context;
mod device;
mod event;
mod logging;
mod platform_listener;
mod plugin;
mod tls;
mod utils;

pub enum CustomWindowEvent {
    ClipboardUpdated,
    PowerStatusUpdated,
    SetTrayMenu(ContextMenu),
    SetTrayIcon(Icon),
}

pub const AUM_ID: &str = "Midori.KDEConnectRS";

#[derive(Debug)]
enum Role {
    Server,
    Client { remote_identity: IdentityPacket },
}

impl Role {
    fn as_str(&self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Client { .. } => "client",
        }
    }
}

/// Broadcasts packets for discovery.
async fn udp_server(tcp_port: u16, ctx: AppContextRef) -> Result<()> {
    let socket = Socket::new(
        Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_broadcast(true)?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;

    let udp_socket = UdpSocket::from_std(socket.into())?;
    let broadcast_addr = (Ipv4Addr::BROADCAST, 1716u16);

    log::info!("UDP server started");

    let mut identity_packet = NetworkPacket::new_identity(
        tcp_port,
        plugin::ALL_CAPS.0.clone(),
        plugin::ALL_CAPS.1.clone(),
        &ctx.config,
    );

    loop {
        if ctx.device_manager.active_device_count() == 0 {
            // Advertise our presence to all devices on the network if we have no active devices.
            identity_packet.reset_ts();
            let buf = serde_json::to_vec(&identity_packet)?;
            udp_socket.send_to(&buf, broadcast_addr).await?;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Handle incoming discovery packets.
async fn handle_udp_packet(buf: &[u8], addr: SocketAddr, ctx: &AppContextRef) -> Result<()> {
    let remote_identity_packet = serde_json::from_slice::<NetworkPacket>(buf)?;
    if remote_identity_packet.typ != packet::PACKET_TYPE_IDENTITY {
        bail!("Invalid packet type: {:?}", remote_identity_packet.typ);
    }

    let remote_identity = remote_identity_packet.into_body::<IdentityPacket>()?;

    if remote_identity.device_id == ctx.config.uuid {
        // Don't connect to ourself.
        return Ok(());
    }
    if ctx.device_manager.query_device(&remote_identity.device_id).await? {
        // Don't connect to devices we're already connected to.
        return Ok(());
    }

    let tcp_port = remote_identity
        .tcp_port
        .ok_or_else(|| anyhow::anyhow!("No TCP port"))?;

    let stream = TcpStream::connect((addr.ip(), tcp_port)).await?;

    let ctx = ctx.clone();
    tokio::spawn(async move {
        let r = handle_conn(Role::Client { remote_identity }, stream, addr.ip(), ctx).await;
        match r {
            Ok(_) => {
                log::info!("Connection from {} closed", addr);
            }
            Err(err) => {
                log::error!("Error handling connection: {:?}", err);
            }
        }
    });

    Ok(())
}

/// Listen to incoming discovery packets.
async fn udp_listener(ctx: AppContextRef) -> Result<()> {
    let socket = Socket::new(
        Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_broadcast(true)?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::from(SocketAddr::new(
        Ipv4Addr::UNSPECIFIED.into(),
        1716u16,
    )))?;

    let udp_socket = UdpSocket::from_std(socket.into())?;

    log::info!("UDP listener started");

    let mut buf = vec![0u8; 1024 * 512];
    loop {
        let (n, addr) = udp_socket.recv_from(&mut buf).await?;

        if let Err(e) = handle_udp_packet(&buf[..n], addr, &ctx).await {
            log::error!("Error handling UDP packet: {}", e);
        }
    }
}

/// Opens a TCP listener on an empty port.
async fn open_tcp_server() -> Result<(TcpListener, u16)> {
    const MIN_PORT: u16 = 1716;
    const MAX_PORT: u16 = 1764;

    let mut last_error = None;

    for port in MIN_PORT..=MAX_PORT {
        let addr = (Ipv4Addr::UNSPECIFIED, port);
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok((listener, port)),
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error.unwrap().into())
}

/// Opens a TCP listener on an empty port for payload serving.
async fn open_payload_tcp_server() -> Result<(TcpListener, u16)> {
    const MIN_PORT: u16 = 1765;

    let mut last_error = None;

    for port in MIN_PORT.. {
        let addr = (Ipv4Addr::UNSPECIFIED, port);
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok((listener, port)),
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error.unwrap().into())
}

/// Serve payload data on the given listener.
async fn serve_payload(server: TcpListener, data: Arc<Vec<u8>>, ctx: AppContextRef) {
    let task = async move {
        loop {
            let (stream, addr) = match server.accept().await {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Error accepting payload connection: {:?}", e);
                    break;
                }
            };

            log::info!("Payload connection from {}", addr);
            let data = data.clone();
            let acceptor = ctx.tls_acceptor();

            tokio::spawn(async move {
                let mut stream = match acceptor.accept(stream).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        log::error!("Failed to accept payload TLS connection: {}", e);
                        return;
                    }
                };

                if let Err(err) = stream.write_all(&data).await {
                    log::error!("Error writing payload to {}: {:?}", addr, err);
                    return;
                }

                if let Err(e) = stream.flush().await {
                    log::error!("Error flushing payload to {}: {:?}", addr, e);
                }
            });
        }
    };

    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .ok();
}

async fn send_packet<W: AsyncWrite + Unpin>(
    mut stream: W,
    mut packet: NetworkPacketWithPayload,
    ctx: AppContextRef,
) -> Result<()> {
    if let Some(payload) = packet.payload {
        match open_payload_tcp_server().await {
            Ok((payload_server, payload_port)) => {
                packet.packet.set_payload(payload.len() as _, payload_port);

                log::info!(
                    "Serving a payload of {} bytes on {}",
                    payload.len(),
                    payload_port
                );

                let ctx = ctx.clone();
                tokio::spawn(async move {
                    serve_payload(payload_server, payload, ctx).await;
                });
            }
            Err(e) => {
                log::error!("Failed to start payload server: {:?}", e);
            }
        }
    }

    let mut bytes = packet.packet.to_vec();
    bytes.push(0x0A);

    stream
        .write_all(&bytes)
        .await
        .context("Write to connection")?;
    stream.flush().await.context("Flush connection")?;

    Ok(())
}

async fn handle_conn(role: Role, stream: TcpStream, ip: IpAddr, ctx: AppContextRef) -> Result<()> {
    let s2_socket = Socket::from(stream.into_std()?);
    // enable keepalive
    s2_socket.set_keepalive(true)?;
    s2_socket.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            // time to start sending keepalive packets (seconds)
            .with_time(Duration::from_secs(10))
            // interval between keepalive packets after the initial period (seconds)
            .with_interval(Duration::from_secs(5)),
    )?;
    let mut stream = TcpStream::from_std(s2_socket.into())?;

    let role_text = role.as_str();

    let (stream, remote_identity) = match role {
        Role::Server => {
            let mut remote_identity = vec![];
            loop {
                let b = stream.read_u8().await?;
                if b == 0x0A {
                    break;
                }
                remote_identity.push(b);
            }

            let remote_identity_packet: NetworkPacket = serde_json::from_slice(&remote_identity)?;
            if remote_identity_packet.typ != packet::PACKET_TYPE_IDENTITY {
                bail!("Invalid packet type: {:?}", remote_identity_packet.typ);
            }
            let remote_identity = remote_identity_packet.into_body::<IdentityPacket>()?;

            (
                tokio_rustls::TlsStream::from(
                    ctx.tls_connector()
                        .connect(ServerName::IpAddress(ip), stream)
                        .await
                        .context("TLS connect")?,
                ),
                remote_identity,
            )
        }
        Role::Client { remote_identity } => {
            let local_identity_packet = NetworkPacket::new_identity(
                None,
                plugin::ALL_CAPS.0.clone(),
                plugin::ALL_CAPS.1.clone(),
                &ctx.config,
            );
            stream.write_all(&local_identity_packet.to_vec()).await?;
            stream.write_all(b"\n").await?;

            (
                tokio_rustls::TlsStream::from(
                    ctx.tls_acceptor()
                        .accept(stream)
                        .await
                        .context("TLS accept")?,
                ),
                remote_identity,
            )
        }
    };

    let device_id = remote_identity.device_id.as_str();
    let _peer_cert = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first());

    let mut stream = BufStream::new(stream);

    log::info!(
        "Handshake successful for {} ({}) at {} as {}",
        remote_identity.device_name,
        device_id,
        ip,
        role_text
    );

    let (conn_id, mut packet_rx, device_handle) = ctx
        .device_manager
        .add_device(device_id, &remote_identity.device_name, ip)
        .await?;

    loop {
        let mut line = String::new();

        tokio::select! {
            packet = packet_rx.recv() => {
                // Send packet
                if let Some(packet) = packet {
                    if let Err(e) = send_packet(&mut stream, packet, ctx.clone()).await {
                        log::error!("Error sending packet to {}: {:?}", ip, e);
                        break;
                    }
                } else {
                    log::info!("Device {} packet sender disconnected", device_id);
                    break;
                }
            }

            read_result = stream.read_line(&mut line) => {
                // Receive packet
                match read_result {
                    Ok(0) => {
                        log::warn!("Connection closed (EOF)");
                        break;
                    }
                    Err(e) => {
                        log::error!("Failed to read from connection: {:?}", e);
                        break;
                    }
                    Ok(_) => {
                        // We have actual data to process
                    }
                }

                match serde_json::from_str::<NetworkPacket>(&line) {
                    Ok(packet) => match packet.typ.as_str() {
                        packet::PACKET_TYPE_PAIR => {
                            // Directly handle pairing requests
                            NetworkPacket::new_pair(true)
                                .write_to_conn(&mut stream)
                                .await?;
                            log::info!("Accepted pairing request");
                        }
                        _ => {
                            device_handle.dispatch_packet(packet).await;
                        },
                    },
                    Err(err) => {
                        log::error!("Failed to parse packet: {:?}", err);
                    }
                }
            }
        }

        if let Err(e) = stream.flush().await {
            log::error!("Failed to flush stream: {:?}", e);
            break;
        }
    }

    // Wait for some time before removing device and notify the user.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    ctx.device_manager.remove_device(device_id, conn_id).await;

    Ok(())
}

async fn tcp_server(listener: TcpListener, ctx: AppContextRef) -> Result<()> {
    log::info!("TCP server started");

    loop {
        let (stream, addr) = listener.accept().await?;

        let ctx = ctx.clone();

        tokio::spawn(async move {
            let r = handle_conn(Role::Server, stream, addr.ip(), ctx).await;
            match r {
                Ok(_) => {
                    log::info!("Connection from {} closed", addr);
                }
                Err(err) => {
                    log::error!("Error handling connection: {:?}", err);
                }
            }
        });
    }
}

async fn event_handler(mut rx: event::EventReceiver, ctx: AppContextRef) {
    let mut last_message = None;

    loop {
        tokio::select! {
            message = rx.recv() => {
                if let Some(current_message) = message {
                    if last_message == Some(current_message) {
                        // The message has been received twice in a row, ignore it.
                        continue;
                    }

                    // The message has changed, send the last one and store the new one.

                    if let Some(last_message) = last_message.take() {
                        ctx.device_manager.broadcast_event(last_message).await;
                    }

                    last_message = Some(current_message);
                } else {
                    return;
                }
            }
            // Wait for 100ms before sending the message.
            _ = tokio::time::sleep(Duration::from_millis(100)), if last_message.is_some() => {
                // Send the last message and clear it.
                ctx.device_manager.broadcast_event(last_message.take().unwrap()).await;
            }
        };
    }
}

#[tokio::main]
async fn server_main(
    event_channel: (event::EventSender, event::EventReceiver),
    event_loop_proxy: EventLoopProxy<CustomWindowEvent>,
    hotkey_manager: ShortcutManager,
) -> Result<()> {
    let (_, event_rx) = event_channel;
    let (tcp_listener, tcp_port) = open_tcp_server().await?;

    log::info!("TCP port: {}", tcp_port);

    let config = config::Config::init_or_load("./config.json")?;

    let ctx = context::ApplicationContext::new(config, event_loop_proxy, hotkey_manager)
        .await
        .context("Initialize context")?;

    // Use the same certificate when we are acting as client and server.

    let client_config = ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(Arc::new(tls::ServerVerifier::AlwaysOk))
        .with_single_cert(
            vec![tokio_rustls::rustls::Certificate(
                ctx.config.tls_cert.clone(),
            )],
            tokio_rustls::rustls::PrivateKey(ctx.config.tls_key.clone()),
        )?;

    let server_config = ServerConfig::builder()
        .with_safe_defaults()
        .with_client_cert_verifier(Arc::new(tls::ClientVerifier::AlwaysOk))
        .with_single_cert(
            vec![tokio_rustls::rustls::Certificate(
                ctx.config.tls_cert.clone(),
            )],
            tokio_rustls::rustls::PrivateKey(ctx.config.tls_key.clone()),
        )?;

    let tls_connector = TlsConnector::from(Arc::new(client_config));
    let tls_acceptor = TlsAcceptor::from(Arc::new(server_config));
    ctx.setup_tls(tls_acceptor, tls_connector);

    let uctx = ctx.clone();
    let udp_task = tokio::spawn(async move {
        let e = udp_server(tcp_port, uctx).await;
        log::warn!("UDP server exited with {:?}", e);
    });

    let uctx = ctx.clone();
    let udp_listener_task = tokio::spawn(async move {
        let e = udp_listener(uctx).await;
        log::warn!("UDP listener exited with {:?}", e);
    });

    let ectx = ctx.clone();
    let event_task = tokio::spawn(async move {
        event_handler(event_rx, ectx).await;
        log::warn!("Event handler exited");
    });

    let tcp_task = tokio::spawn(async move {
        let e = tcp_server(tcp_listener, ctx).await;
        log::warn!("TCP server exited with {:?}", e);
    });

    udp_task.await?;
    udp_listener_task.await?;
    tcp_task.await?;
    event_task.await?;

    Ok(())
}

fn main() -> Result<()> {
    logging::setup_logger().expect("Failed to set up logger");

    let (event_tx, event_rx) = mpsc::channel(10);

    let base_dirs = directories::BaseDirs::new().expect("Failed to get base dirs");
    let data_dir = base_dirs.data_dir().join("kde-connect-rs");
    std::fs::create_dir_all(&data_dir)?;

    {
        let icon_path = data_dir.join("notification.ico");
        if !icon_path.exists() {
            // Extract icon from executable
            let mut icon_file = std::fs::File::create(&icon_path)?;
            icon_file.write_all(include_bytes!("icons/notification.ico"))?;
        }
        winrt_toast::register(AUM_ID, "KDE Connect", Some(&icon_path))?;
    }

    platform_listener::mpris::start(event_tx.clone())?;

    let event_loop: EventLoop<CustomWindowEvent> = EventLoop::with_user_event();

    let icon = Icon::from_rgba(vec![0; 32 * 32 * 4], 32, 32).unwrap();
    let mut system_tray = SystemTrayBuilder::new(icon, None)
        .with_tooltip("KDE Connect")
        .build(&event_loop)
        .unwrap();

    let hotkey_manager = ShortcutManager::new(&event_loop);

    let windows_listener = platform_listener::windows::WindowsListener::new(&event_loop)?;

    let window = WindowBuilder::new()
        .with_title("KDEConnect.rs")
        .with_visible(false)
        .build(&event_loop)
        .unwrap();

    let event_tx_main = event_tx.clone();
    let proxy = event_loop.create_proxy();
    std::thread::spawn(|| {
        let r = server_main((event_tx_main, event_rx), proxy, hotkey_manager);
        if let Err(e) = r {
            log::error!("Server exited with error: {}", e);
        }
    });

    event_loop.run(move |event, _, control_flow| {
        let _ = windows_listener;

        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                window_id,
                ..
            } if window_id == window.id() => *control_flow = ControlFlow::Exit,
            Event::MainEventsCleared => {
                window.request_redraw();
            }
            // Event::GlobalShortcutEvent(hotkey_id) if hotkey_id == shortcut_1.clone().id() => {
            //     println!("Pressed `shortcut_1` -- unregister for future use");
            //     // unregister key
            //     hotkey_manager
            //         .unregister(global_shortcut_1.clone())
            //         .unwrap();
            // }
            // Event::GlobalShortcutEvent(hotkey_id) if hotkey_id == shortcut_2.clone().id() => {
            //     println!("Pressed on `shortcut_2`");
            // }
            // // you can match hotkey_id with accelerator_string only if you used `from_str`
            // // by example `shortcut_1` will NOT match AcceleratorId::new("SHIFT+UP") as it's
            // // been created with a struct and the ID is generated automatically
            // Event::GlobalShortcutEvent(hotkey_id)
            //     if hotkey_id == AcceleratorId::new("COMMANDORCONTROL+SHIFT+3") =>
            // {
            //     println!("Pressed on `shortcut_3`");
            // }
            // Event::GlobalShortcutEvent(hotkey_id) if hotkey_id == shortcut_4.clone().id() => {
            //     println!("Pressed on `shortcut_4`");
            // }
            Event::GlobalShortcutEvent(hotkey_id) => {
                println!("hotkey_id {:?}", hotkey_id);
            }
            Event::MenuEvent {
                menu_id, origin, ..
            } if origin == MenuType::ContextMenu => {
                event_tx
                    .blocking_send(event::SystemEvent::TrayMenuClicked(menu_id))
                    .ok();
            }
            Event::UserEvent(event) => match event {
                CustomWindowEvent::ClipboardUpdated => {
                    event_tx
                        .blocking_send(event::SystemEvent::ClipboardUpdated)
                        .ok();
                }
                CustomWindowEvent::PowerStatusUpdated => {
                    event_tx
                        .blocking_send(event::SystemEvent::PowerStatusUpdated)
                        .ok();
                }
                CustomWindowEvent::SetTrayMenu(menu) => {
                    system_tray.set_menu(&menu);
                }
                CustomWindowEvent::SetTrayIcon(icon) => {
                    system_tray.set_icon(icon);
                }
            },
            _ => {}
        }
    });
}
