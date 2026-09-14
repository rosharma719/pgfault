use pgfault_events::StartupMetadata;
use pgfault_proxy::{serve, Config};
use pgfault_scenario::{
    Action, Disconnect, Matcher, Mode, Scenario, Selector, Side, Suppress, When,
};
use pgfault_trace::Recorder;
use pgfault_wire::{frame, FrameReader};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

fn commit_scenario() -> Scenario {
    Scenario {
        version: 1,
        name: "commit".into(),
        selector: Selector::default(),
        when: When {
            event: "transaction.commit.completed".into(),
            occurrence: Some(1),
            statement_class: vec![],
            row: None,
        },
        action: Action {
            suppress: Some(Suppress { current: true }),
            disconnect: Some(Disconnect {
                side: Side::Frontend,
                mode: Mode::Reset,
            }),
            ..Action::default()
        },
    }
}
async fn fixture(
    scenarios: Vec<Scenario>,
) -> (
    TcpStream,
    TcpStream,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
) {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let trace = Recorder::create(dir.path().join("trace.jsonl")).unwrap();
    let config = Config {
        upstream: upstream.local_addr().unwrap().to_string(),
        scenarios,
        trace,
        tls: Default::default(),
    };
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        serve(listener, config, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    let mut client = TcpStream::connect(address).await.unwrap();
    client
        .write_all(&[0, 0, 0, 8, 4, 210, 22, 47])
        .await
        .unwrap();
    let mut no = [0];
    client.read_exact(&mut no).await.unwrap();
    assert_eq!(&no, b"N");
    let startup = b"\0\0\0\x17\0\x03\0\0user\0postgres\0\0";
    client.write_all(startup).await.unwrap();
    let (mut server, _) = upstream.accept().await.unwrap();
    let mut actual = vec![0; startup.len()];
    server.read_exact(&mut actual).await.unwrap();
    assert_eq!(actual, startup);
    server.write_all(&frame(b'Z', b"I").0).await.unwrap();
    let mut ready = [0; 6];
    client.read_exact(&mut ready).await.unwrap();
    (client, server, tx, task, dir)
}
#[tokio::test]
async fn commit_acknowledgement_never_leaks_before_idle_confirmation() {
    let (mut client, mut server, shutdown, task, _dir) = fixture(vec![commit_scenario()]).await;
    let mut reader = FrameReader::default();
    client.write_all(&frame(b'Q', b"COMMIT\0").0).await.unwrap();
    assert_eq!(reader.next(&mut server).await.unwrap().unwrap().tag(), b'Q');
    server.write_all(&frame(b'C', b"COMMIT\0").0).await.unwrap();
    let mut byte = [0];
    assert!(
        tokio::time::timeout(Duration::from_millis(50), client.read(&mut byte))
            .await
            .is_err()
    );
    server.write_all(&frame(b'N', b"notice\0").0).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), client.read(&mut byte))
            .await
            .is_err()
    );
    server.write_all(&frame(b'Z', b"I").0).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), client.read(&mut byte))
        .await
        .unwrap();
    assert!(
        result.is_err() || result.unwrap() == 0,
        "client received commit evidence"
    );
    shutdown.send(()).unwrap();
    task.await.unwrap();
}
#[tokio::test]
async fn no_fault_unknown_frames_and_fragmented_auth_are_byte_exact() {
    let (mut client, mut server, shutdown, task, _dir) = fixture(vec![]).await;
    let unknown = frame(b'?', b"\0\xff\x01unknown");
    for b in unknown.0.iter() {
        client.write_all(&[*b]).await.unwrap();
    }
    let mut reader = FrameReader::default();
    assert_eq!(
        reader.next(&mut server).await.unwrap().unwrap().0,
        unknown.0
    );
    let auth = frame(b'R', b"\0\0\0\x0bSCRAMopaque\xff");
    let response = frame(b'!', b"unknown backend");
    for b in auth.0.iter().chain(response.0.iter()) {
        server.write_all(&[*b]).await.unwrap();
    }
    assert_eq!(reader.next(&mut client).await.unwrap().unwrap().0, auth.0);
    assert_eq!(
        reader.next(&mut client).await.unwrap().unwrap().0,
        response.0
    );
    client.write_all(&frame(b'Q', b"COMMIT\0").0).await.unwrap();
    reader.next(&mut server).await.unwrap();
    server.write_all(&frame(b'C', b"COMMIT\0").0).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), reader.next(&mut client))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .tag(),
        b'C'
    );
    shutdown.send(()).unwrap();
    task.await.unwrap();
}
#[test]
fn occurrence_counts_only_selected_events_per_connection() {
    let mut s = commit_scenario();
    s.selector.application_name = Some("chosen".into());
    s.when.occurrence = Some(2);
    let mut m = Matcher::new(s);
    let mut state = pgfault_state::ConnectionState::new(1, StartupMetadata::default());
    state.frontend(&frame(b'Q', b"COMMIT\0"));
    state.backend(&frame(b'C', b"COMMIT\0"));
    let mut e = state
        .backend(&frame(b'Z', b"I"))
        .into_iter()
        .find(|e| e.event == "transaction.commit.completed")
        .unwrap();
    assert!(!m.observe(&e));
    e.startup.application_name = Some("chosen".into());
    assert!(!m.observe(&e));
    assert!(m.observe(&e));
    assert!(!m.observe(&e));
}
