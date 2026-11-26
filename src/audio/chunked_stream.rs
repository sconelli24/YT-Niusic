use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{self, Stream};
use stream_download::http::reqwest as sd_reqwest;
use stream_download::source::{DecodeError, SourceStream};

/// Tamaño de bloque para peticiones Range contra `googlevideo.com` (512 KB).
/// YouTube rechaza con 403 peticiones GET abiertas o de rango completo en clientes móviles,
/// pero sirve bloques <= 1 MB a máxima velocidad sin throttling.
const CHUNK_SIZE: u64 = 512 * 1024;

#[derive(Debug, Clone)]
pub struct ChunkedYtParams {
    pub url: String,
    pub user_agent: String,
    pub content_length: Option<u64>,
}

#[derive(Debug)]
pub struct ChunkedStreamError(pub String);

impl fmt::Display for ChunkedStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChunkedStreamError {}

impl DecodeError for ChunkedStreamError {}

type InnerBoxStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

/// Stream HTTP por bloques `Range` compatible con `stream-download` (`Read + Seek`).
pub struct ChunkedYtStream {
    client: sd_reqwest::Client,
    url: String,
    total_len: u64,
    inner: Mutex<InnerBoxStream>,
}

impl Stream for ChunkedYtStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut guard = self.inner.lock().unwrap();
        guard.as_mut().poll_next(cx)
    }
}

impl SourceStream for ChunkedYtStream {
    type Params = ChunkedYtParams;
    type StreamCreationError = ChunkedStreamError;

    async fn create(params: Self::Params) -> Result<Self, Self::StreamCreationError> {
        let client = sd_reqwest::Client::builder()
            .user_agent(&params.user_agent)
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| ChunkedStreamError(format!("Error creando cliente HTTP: {e}")))?;

        let first_end = params
            .content_length
            .map(|len| len.saturating_sub(1).min(CHUNK_SIZE - 1))
            .unwrap_or(CHUNK_SIZE - 1);

        let resp = client
            .get(&params.url)
            .header(
                sd_reqwest::header::RANGE,
                format!("bytes=0-{first_end}"),
            )
            .send()
            .await
            .map_err(|e| ChunkedStreamError(format!("Error conectando al stream: {e}")))?;

        let status = resp.status();
        if !status.is_success() && status != sd_reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(ChunkedStreamError(format!(
                "El servidor rechazó el stream de audio (HTTP {status})"
            )));
        }

        let total_len = parse_content_range_total(resp.headers())
            .or(params.content_length)
            .or_else(|| resp.content_length())
            .ok_or_else(|| {
                ChunkedStreamError("No se pudo determinar el tamaño del stream de audio".into())
            })?;

        let first_chunk = resp
            .bytes()
            .await
            .map_err(|e| ChunkedStreamError(format!("Error leyendo primer bloque: {e}")))?;

        let next_offset = first_chunk.len() as u64;
        let stream = build_chunk_stream(
            client.clone(),
            params.url.clone(),
            Some(first_chunk),
            next_offset,
            total_len,
        );

        Ok(Self {
            client,
            url: params.url,
            total_len,
            inner: Mutex::new(stream),
        })
    }

    fn content_length(&self) -> Option<u64> {
        Some(self.total_len)
    }

    async fn seek_range(&mut self, start: u64, end: Option<u64>) -> io::Result<()> {
        let limit = end.unwrap_or(self.total_len).min(self.total_len);
        let stream = if start >= limit {
            Box::pin(stream::empty()) as InnerBoxStream
        } else {
            build_chunk_stream(self.client.clone(), self.url.clone(), None, start, limit)
        };
        *self.inner.lock().unwrap() = stream;
        Ok(())
    }

    async fn reconnect(&mut self, current_position: u64) -> io::Result<()> {
        self.seek_range(current_position, None).await
    }

    fn supports_seek(&self) -> bool {
        true
    }
}

struct ChunkState {
    client: sd_reqwest::Client,
    url: String,
    buffered_first: Option<Bytes>,
    offset: u64,
    limit: u64,
}

fn build_chunk_stream(
    client: sd_reqwest::Client,
    url: String,
    buffered_first: Option<Bytes>,
    offset: u64,
    limit: u64,
) -> InnerBoxStream {
    let state = ChunkState {
        client,
        url,
        buffered_first,
        offset,
        limit,
    };

    Box::pin(stream::unfold(state, |mut st| async move {
        if let Some(first) = st.buffered_first.take() {
            if !first.is_empty() {
                return Some((Ok(first), st));
            }
        }

        if st.offset >= st.limit {
            return None;
        }

        let end = (st.offset + CHUNK_SIZE - 1).min(st.limit - 1);
        let range_header = format!("bytes={}-{end}", st.offset);

        let mut last_err: Option<io::Error> = None;
        for attempt in 0..3u8 {
            if attempt > 0 {
                let backoff_ms = 250u64 * (1u64 << (attempt - 1));
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }

            let resp = match st
                .client
                .get(&st.url)
                .header(sd_reqwest::header::RANGE, &range_header)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(io::Error::other(format!("Error HTTP chunk: {e}")));
                    continue;
                }
            };

            let status = resp.status();
            if !status.is_success() && status != sd_reqwest::StatusCode::PARTIAL_CONTENT {
                let err = io::Error::other(format!(
                    "HTTP {status} descargando bloque {range_header}"
                ));
                // 403/404 no se recuperan reintentando el mismo bloque
                if status == sd_reqwest::StatusCode::FORBIDDEN
                    || status == sd_reqwest::StatusCode::NOT_FOUND
                {
                    return Some((Err(err), st));
                }
                last_err = Some(err);
                continue;
            }

            match resp.bytes().await {
                Ok(bytes) if bytes.is_empty() => return None,
                Ok(bytes) => {
                    st.offset += bytes.len() as u64;
                    return Some((Ok(bytes), st));
                }
                Err(e) => {
                    last_err = Some(io::Error::other(format!("Error leyendo bloque: {e}")));
                }
            }
        }

        Some((
            Err(last_err.unwrap_or_else(|| io::Error::other("Error descargando bloque de audio"))),
            st,
        ))
    }))
}

/// Extrae el tamaño total desde una cabecera `Content-Range: bytes 0-524287/5466181`.
fn parse_content_range_total(headers: &sd_reqwest::header::HeaderMap) -> Option<u64> {
    let val = headers.get(sd_reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    let total_str = val.rsplit('/').next()?;
    total_str.trim().parse::<u64>().ok()
}
