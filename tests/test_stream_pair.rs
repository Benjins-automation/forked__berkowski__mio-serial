#![cfg(unix)]

#[test]
fn test_stream_pair() {
    let (mut master, mut slave) = SerialStream::pair().expect("Unable to create ptty pair");

    // Test file descriptors.
    assert!(
        master.as_raw_fd() > 0,
        "Invalid file descriptor on master ptty"
    );
    assert!(
        slave.as_raw_fd() > 0,
        "Invalid file descriptor on slave ptty"
    );
    assert_ne!(
        master.as_raw_fd(),
        slave.as_raw_fd(),
        "master and slave ptty's share the same file descriptor."
    );

    let msg = "Test Message";
    let mut buf = [0u8; 128];

    // Write the string on the master
    assert_eq!(
        master.write(msg.as_bytes()).unwrap(),
        msg.len(),
        "Unable to write message on master."
    );

    // Read it on the slave
    let nbytes = slave.read(&mut buf).expect("Unable to read bytes.");
    assert_eq!(
        nbytes,
        msg.len(),
        "Read message length differs from sent message."
    );

    assert_eq!(
        str::from_utf8(&buf[..nbytes]).unwrap(),
        msg,
        "Received message does not match sent"
    );
}
