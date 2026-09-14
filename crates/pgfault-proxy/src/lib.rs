use anyhow::{bail, Context, Result};
use bytes::Bytes;
use pgfault_events::{Event, StartupMetadata};
use pgfault_scenario::{Action, Matcher, Mode, Scenario, Side};
use pgfault_state::ConnectionState;
use pgfault_trace::{Record, Recorder};
use pgfault_wire::{
    cstr, startup, FrameReader, CANCEL_REQUEST, GSS_REQUEST, PROTOCOL_V3, SSL_REQUEST,
};
use std::{future::Future, net::SocketAddr, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
#[derive(Clone)]
pub struct Config {
    pub upstream: String,
    pub scenarios: Vec<Scenario>,
    pub trace: Recorder,
}
/// Each accepted client owns exactly one upstream and isolated semantic/fault state.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let mut next_id = 0;
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _=&mut shutdown=>break,
            accepted=listener.accept()=> {
                let (frontend,peer)=accepted?;next_id+=1;let id=next_id;let config=config.clone();
                tasks.spawn(async move {
                    let result=connection(frontend,&config,id).await;
                    if let Err(e)=&result {tracing::warn!(connection_id=id,%peer,error=%e,"connection ended");}
                    let _=config.trace.lifecycle(id,"connection.closed",&result.err().map(|e|e.to_string()).unwrap_or_default());
                });
            }
            Some(result)=tasks.join_next(), if !tasks.is_empty()=> {result?;}
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
pub async fn bind(address: &str) -> Result<TcpListener> {
    Ok(TcpListener::bind(address).await?)
}
pub fn local_addr(listener: &TcpListener) -> Result<SocketAddr> {
    Ok(listener.local_addr()?)
}
fn metadata(packet: &[u8]) -> Result<StartupMetadata> {
    let mut m = StartupMetadata::default();
    let mut rest = &packet[8..];
    loop {
        let (key, tail) = cstr(rest).context("invalid startup parameter")?;
        if key.is_empty() {
            if !tail.is_empty() {
                bail!("trailing startup data");
            }
            break;
        }
        let (value, tail) = cstr(tail).context("invalid startup value")?;
        let value = String::from_utf8_lossy(value).into_owned();
        match key {
            b"user" => m.user = Some(value),
            b"database" => m.database = Some(value),
            b"application_name" => m.application_name = Some(value),
            _ => {}
        }
        rest = tail;
    }
    Ok(m)
}
async fn connection(mut frontend: TcpStream, config: &Config, id: u64) -> Result<()> {
    frontend.set_nodelay(true)?;
    let packet = loop {
        let packet =
            tokio::time::timeout(Duration::from_secs(30), startup(&mut frontend)).await??;
        let code = u32::from_be_bytes(packet[4..8].try_into().unwrap());
        match code {
            SSL_REQUEST|GSS_REQUEST=> {if packet.len()!=8 {bail!("invalid negotiation request");}frontend.write_all(b"N").await?;}
            CANCEL_REQUEST=> {
                if packet.len()!=16 {bail!("invalid cancellation request");}
                let mut upstream=TcpStream::connect(&config.upstream).await?;
                upstream.write_all(&packet).await?;upstream.shutdown().await?;
                config.trace.lifecycle(id,"cancel.forwarded","")?;return Ok(());
            }
            PROTOCOL_V3=>break packet,
            _=>bail!("unsupported startup protocol {code}; use PostgreSQL protocol 3.0 and sslmode=disable"),
        }
    };
    let meta = metadata(&packet)?;
    let mut upstream = TcpStream::connect(&config.upstream)
        .await
        .context("connecting upstream")?;
    upstream.set_nodelay(true)?;
    upstream.write_all(&packet).await?;
    let mut state = ConnectionState::new(id, meta);
    config.trace.lifecycle(id, "startup", "")?;
    let mut matchers: Vec<_> = config.scenarios.iter().cloned().map(Matcher::new).collect();
    let mut client_reader = FrameReader::default();
    let mut server_reader = FrameReader::default();
    let mut held: Vec<Bytes> = Vec::new();
    let mut held_size = 0usize;
    let mut truncate = None;
    loop {
        tokio::select! {
            frame=client_reader.next(&mut frontend)=> {
                let Some(f)=frame? else {upstream.shutdown().await?;return Ok(());};
                let events=state.frontend(&f);
                let disposition=apply(events,&mut matchers,&config.trace,&mut frontend,&mut upstream,&mut truncate).await?;
                if disposition.disconnect {return Ok(());}
                if !disposition.suppress {upstream.write_all(&f.0).await?;
                    if matches!(f.tag(),b'Q'|b'E') {
                        let d=apply(vec![state.forwarded()],&mut matchers,&config.trace,&mut frontend,&mut upstream,&mut truncate).await?;
                        if d.disconnect {return Ok(());}
                    }
                }
                if f.tag()==b'X' {return Ok(());}
            }
            frame=server_reader.next(&mut upstream)=> {
                let Some(f)=frame? else {frontend.shutdown().await?;return Ok(());};
                let gate=state.completion_boundary(&f).is_some_and(|b|matchers.iter().any(|m|m.needs_gate(b)));
                let events=state.backend(&f);
                let d=apply(events,&mut matchers,&config.trace,&mut frontend,&mut upstream,&mut truncate).await?;
                if d.disconnect {return Ok(());}
                // A result truncation delivers exactly N complete rows, then closes before row N+1.
                let cut=if f.tag()==b'D' {if let Some(left)=truncate.as_mut() {if *left==0 {true} else {*left-=1;false}} else {false}} else {false};
                if cut {reset(&frontend)?;config.trace.lifecycle(id,"result.truncated","")?;return Ok(());}
                if f.tag()==b'Z' {
                    if !d.suppress {for bytes in held.drain(..) {frontend.write_all(&bytes).await?;}frontend.write_all(&f.0).await?;} else {held.clear();}
                    held_size=0;truncate=None;
                } else if !d.suppress {
                    if gate || !held.is_empty() {
                        held_size+=f.0.len();if held_size>1024*1024 {bail!("completion gate exceeded 1 MiB; unsupported pipelined completion");}
                        held.push(f.0.clone());
                    } else {frontend.write_all(&f.0).await?;}
                }
                if f.tag()==b'D' && truncate==Some(0) {reset(&frontend)?;config.trace.lifecycle(id,"result.truncated","")?;return Ok(());}
            }
        }
    }
}
fn reset(socket: &TcpStream) -> Result<()> {
    socket2::SockRef::from(socket).set_linger(Some(Duration::ZERO))?;
    Ok(())
}
#[derive(Default)]
struct Disposition {
    suppress: bool,
    disconnect: bool,
}
async fn apply(
    events: Vec<Event>,
    matchers: &mut [Matcher],
    trace: &Recorder,
    frontend: &mut TcpStream,
    upstream: &mut TcpStream,
    truncate: &mut Option<u64>,
) -> Result<Disposition> {
    let mut d = Disposition::default();
    for e in events {
        trace.event(e.clone())?;
        for m in matchers.iter_mut() {
            if !m.observe(&e) {
                continue;
            }
            trace.record(Record::Fault {
                coordinate: e.clone(),
                scenario: m.scenario.clone(),
            })?;
            tracing::info!(connection_id=e.connection_id,event=%e.event,scenario=%m.scenario.name,"FAULT FIRED");
            let Action {
                suppress,
                disconnect,
                delay,
                truncate_result,
            } = &m.scenario.action;
            if let Some(delay) = delay {
                tokio::time::sleep(humantime_duration(&delay.duration)?).await;
            }
            if suppress.as_ref().is_some_and(|s| s.current) {
                d.suppress = true;
            }
            if let Some(t) = truncate_result {
                *truncate = Some(t.after_rows);
            }
            if let Some(disconnect) = disconnect {
                let socket = match disconnect.side {
                    Side::Frontend => &mut *frontend,
                    Side::Upstream => &mut *upstream,
                };
                match disconnect.mode {
                    Mode::Reset => reset(socket)?,
                    Mode::Close => socket.shutdown().await?,
                }
                d.disconnect = true;
            }
        }
    }
    Ok(d)
}
fn humantime_duration(s: &str) -> Result<Duration> {
    Ok(humantime::parse_duration(s)?)
}
