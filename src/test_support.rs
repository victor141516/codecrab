use tokio::{io::AsyncReadExt, net::TcpStream};

pub(crate) async fn read_http_request(socket: &mut TcpStream) -> Vec<u8> {
    const MAXIMUM_REQUEST_BYTES: usize = 4 * 1024 * 1024;

    let mut request = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let read = socket.read(&mut buffer).await.unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        assert!(
            request.len() <= MAXIMUM_REQUEST_BYTES,
            "test HTTP request exceeded {MAXIMUM_REQUEST_BYTES} bytes"
        );
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        });
        if content_length.is_none_or(|length| request.len() >= header_end + length) {
            break;
        }
    }
    request
}
