use super::*;

fn args(list: &[&str]) -> impl Iterator<Item = String> {
    list.iter()
        .map(|arg| arg.to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

#[test]
fn options() {
    assert_eq!(parse(args(&[])).unwrap(), Options::default());
    let options = parse(args(&["--dir", "/tmp/p", "--allow-write", "--allow-bash"])).unwrap();
    assert_eq!(options.dir.as_deref(), Some(std::path::Path::new("/tmp/p")));
    assert!(options.allow_write && options.allow_bash && !options.allow_home);
    assert_eq!(
        parse(args(&["--dir=/x", "--allow-home"])).unwrap().dir,
        Some(PathBuf::from("/x"))
    );
    assert!(parse(args(&["--help"])).unwrap().help);
    assert!(parse(args(&["--dir"])).is_err());
    assert!(parse(args(&["--listen", "x"])).is_err());
}

#[tokio::test]
async fn lines_are_read_whole_and_long_ones_dropped() {
    let mut text = b"{\"a\":1}\r\n\n".to_vec();
    text.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES + 10));
    text.extend(b"\n{\"b\":2}");
    let mut reader = tokio::io::BufReader::with_capacity(64, &text[..]);
    assert_eq!(read_line(&mut reader).await, Ok(Some("{\"a\":1}".into())));
    assert_eq!(read_line(&mut reader).await, Ok(Some(String::new())));
    assert!(matches!(
        read_line(&mut reader).await,
        Err(ReadError::Dropped(why)) if why.contains("dropped")
    ));
    assert_eq!(read_line(&mut reader).await, Ok(Some("{\"b\":2}".into())));
    assert_eq!(read_line(&mut reader).await, Ok(None));
}

#[tokio::test]
async fn a_failing_stdin_is_an_io_error() {
    struct Broken;
    impl tokio::io::AsyncRead for Broken {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("gone")))
        }
    }
    let mut reader = tokio::io::BufReader::new(Broken);
    assert!(
        matches!(read_line(&mut reader).await, Err(ReadError::Io(why)) if why.contains("gone"))
    );
}
