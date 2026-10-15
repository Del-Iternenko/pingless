use super::*;
use std::net::SocketAddr;

#[test]
fn data_roundtrip_both_directions() {
    let keys = SessionKeys::from_psk("secret");
    let client = keys.session(42, false);
    let relay = keys.session(42, true);
    let peer: SocketAddr = "[2001:db8::1]:27015".parse().unwrap();
    let body = Body::Data(DataBody {
        flow: 7,
        seq: 3,
        peer,
        payload: b"hello",
    });

    let mut buf = Vec::new();
    client.seal(&body, &mut buf).unwrap();
    let (h, got) = relay.open(&mut buf).unwrap();
    assert_eq!(h.session, 42);
    assert_eq!(got, body);

    let pong = Body::Pong(PingBody {
        path: 1,
        probe: 9,
        sent_us: 123,
    });
    relay.seal(&pong, &mut buf).unwrap();
    assert_eq!(client.open(&mut buf).unwrap().1, pong);
}

#[test]
fn rejects_tampering_reflection_and_wrong_key() {
    let keys = SessionKeys::from_psk("secret");
    let client = keys.session(1, false);
    let relay = keys.session(1, true);
    let mut buf = Vec::new();
    client.seal(&Body::Close { flow: 1 }, &mut buf).unwrap();

    // A client packet reflected back at the client must not be accepted.
    assert_eq!(client.open(&mut buf.clone()).unwrap_err(), Error::Auth);

    let mut bad = buf.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert_eq!(relay.open(&mut bad).unwrap_err(), Error::Auth);

    let other = SessionKeys::from_psk("other").session(1, true);
    assert_eq!(other.open(&mut buf.clone()).unwrap_err(), Error::Auth);

    // Same key, different session id.
    let wrong_session = keys.session(2, true);
    assert_eq!(wrong_session.open(&mut buf).unwrap_err(), Error::Auth);
}

#[test]
fn window_dedups_and_handles_reordering() {
    let mut w = SeqWindow::default();
    assert!(w.accept(1));
    assert!(!w.accept(1));
    assert!(w.accept(3));
    assert!(w.accept(2), "late but inside the window");
    assert!(!w.accept(2));
    assert!(w.accept(5000));
    assert!(!w.accept(3), "too old");
    assert!(w.accept(4999));
    assert!(!w.accept(0));
    // A jump bigger than the window must clear stale bits.
    assert!(w.accept(5000 + 1024));
    assert!(w.accept(5000 + 1023));
}
