//! Classic connection selection and response-committed media lifecycle.

use super::*;

pub(super) async fn dispatch(
    request: &Request,
    peer: SocketAddr,
    local: SocketAddr,
    config: &ServerConfig,
    pairing_limiter: &PairSetupLimiter,
    next_stream_id: &AtomicU64,
    state: &mut ConnectionState,
) -> DispatchOutcome {
    if has_ambiguous_cseq(request) {
        return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
    }
    let pair =
        request.method() == "POST" && matches!(request.target(), "/pair-setup" | "/pair-pin-start");
    let announce = request.method() == "ANNOUNCE";
    if (state.protocol == ConnectionProtocol::Classic && (pair || state.control.is_some()))
        || (state.protocol == ConnectionProtocol::AirPlay2 && announce)
    {
        return DispatchOutcome::close(response_for(
            request,
            455,
            "Method Not Valid in This State",
        ));
    }
    if state.protocol == ConnectionProtocol::Undecided {
        if pair || state.control.is_some() {
            state.protocol = ConnectionProtocol::AirPlay2;
        } else if announce {
            state.protocol = ConnectionProtocol::Classic;
        }
    }
    if state.protocol == ConnectionProtocol::Classic {
        return dispatch_classic(request, peer, local, config, next_stream_id, state).await;
    }
    let stream = state.active_stream_id();
    let kind = state.active_media_kind();
    dispatch_request(
        request,
        state.control.is_some(),
        peer,
        local,
        config,
        pairing_limiter,
        &mut state.pairing,
        &mut state.event_keys,
        state.event_task.is_some(),
        &mut state.phase_one,
        state.media_permit.is_some(),
        stream,
        kind,
        next_stream_id,
        state.connection_id,
    )
    .await
}

fn unique_header<'a>(
    request: &'a Request,
    name: &str,
    limit: usize,
) -> Result<Option<&'a [u8]>, ()> {
    let mut values = request
        .headers()
        .iter()
        .filter(|h| h.name().eq_ignore_ascii_case(name));
    let value = values.next().map(Header::value);
    if values.next().is_some() || value.is_some_and(|v| v.is_empty() || v.len() > limit) {
        return Err(());
    }
    Ok(value)
}

fn authorize(
    request: &Request,
    config: &ServerConfig,
    state: &mut ClassicConnection,
) -> Result<(), DispatchOutcome> {
    let Some(password) = config.password.as_deref() else {
        state.authenticated = true;
        return Ok(());
    };
    if state.authenticated {
        return Ok(());
    }
    let header = unique_header(request, "Authorization", 4096)
        .map_err(|_| DispatchOutcome::close(response_for(request, 400, "Bad Request")))?;
    let result = state
        .challenge
        .as_ref()
        .zip(header)
        .map(|(challenge, header)| {
            challenge.verify(password, request.method(), request.target(), header)
        });
    if matches!(result, Some(Ok(()))) {
        state.authenticated = true;
        state.auth_failures = 0;
        return Ok(());
    }
    if state.challenge.is_none() || matches!(result, Some(Err(DigestError::ExpiredChallenge))) {
        state.challenge = Some(DigestChallenge::new());
    }
    state.auth_failures = state.auth_failures.saturating_add(1);
    let mut outcome = DispatchOutcome::reply(response_for(request, 401, "Unauthorized"));
    outcome.response.headers.push(
        Header::new(
            "WWW-Authenticate",
            state
                .challenge
                .as_ref()
                .expect("issued challenge")
                .header_value(),
        )
        .expect("safe generated challenge"),
    );
    outcome.close_after = state.auth_failures >= 8;
    Err(outcome)
}

async fn dispatch_classic(
    request: &Request,
    peer: SocketAddr,
    local: SocketAddr,
    config: &ServerConfig,
    next_stream_id: &AtomicU64,
    state: &mut ConnectionState,
) -> DispatchOutcome {
    if request.method() == "OPTIONS" {
        return options_response(request);
    }
    if request.method() == "GET" && request.target() == "/info" {
        return info_response(request, config);
    }
    if (request.method() == "ANNOUNCE" || !state.classic.authenticated)
        && !config.classic_attempts.allow(peer.ip())
    {
        let mut outcome = DispatchOutcome::close(response_for(request, 429, "Too Many Requests"));
        outcome
            .response
            .headers
            .push(Header::new("Retry-After", "1").expect("constant header"));
        return outcome;
    }
    if let Err(outcome) = authorize(request, config, &mut state.classic) {
        return outcome;
    }
    if let Some(session) = state.classic.session.as_deref() {
        match unique_header(request, "Session", 128) {
            Ok(None) => {} // One authenticated TCP connection owns one session.
            Ok(Some(value))
                if std::str::from_utf8(value).ok().is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|id| id.trim() == session)
                }) => {}
            _ => return DispatchOutcome::reply(response_for(request, 454, "Session Not Found")),
        }
    }
    match request.method() {
        "ANNOUNCE" => {
            if state.classic.announced.is_some()
                || state.classic.session.is_some()
                || state.media.is_some()
            {
                return DispatchOutcome::reply(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            }
            if !has_content_type(request, "application/sdp") {
                return DispatchOutcome::reply(response_for(
                    request,
                    415,
                    "Unsupported Media Type",
                ));
            }
            let announcement = match Announcement::parse(request.body()) {
                Ok(value) => value,
                Err(_) => return DispatchOutcome::close(response_for(request, 400, "Bad Request")),
            };
            let permit = match config.classic_key_permits.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        503,
                        "Service Unavailable",
                    ))
                }
            };
            let provider = Arc::clone(&config.classic_key);
            let Announcement {
                wrapped_key,
                iv,
                min_latency,
                max_latency,
            } = announcement;
            let key = match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                provider.unwrap_key(wrapped_key.as_slice())
            })
            .await
            {
                Ok(Ok(key)) => key,
                Ok(Err(super::super::airport_express::ClassicKeyError::InvalidCiphertext)) => {
                    return DispatchOutcome::close(response_for(request, 403, "Forbidden"))
                }
                _ => {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ))
                }
            };
            let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
            outcome.install_classic_announcement = Some(ClassicAudioConfig {
                key,
                iv,
                min_latency,
                max_latency,
            });
            outcome
        }
        "SETUP" => {
            if state.media.is_some()
                || state.classic.session.is_some()
                || state.classic.announced.is_none()
            {
                return DispatchOutcome::reply(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            }
            let transport = match unique_header(request, "Transport", 1024) {
                Ok(Some(value)) => match ClassicTransport::parse(value) {
                    Ok(value) => value,
                    Err(_) => {
                        return DispatchOutcome::reply(response_for(
                            request,
                            461,
                            "Unsupported Transport",
                        ))
                    }
                },
                _ => return DispatchOutcome::reply(response_for(request, 400, "Bad Request")),
            };
            let permit = match config.media_permits.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    return DispatchOutcome::reply(response_for(
                        request,
                        453,
                        "Not Enough Bandwidth",
                    ))
                }
            };
            let audio = state
                .classic
                .announced
                .take()
                .expect("checked announcement");
            let media = match PreparedClassic::prepare(local, peer, transport, audio).await {
                Ok(media) => media,
                Err(_) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ))
                }
            };
            let ports = media.ports();
            let stream_id = next_stream_id.fetch_add(1, Ordering::Relaxed);
            let session = uuid::Uuid::new_v4().simple().to_string();
            let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
            outcome.response.headers.push(Header::new("Transport", format!("RTP/AVP/UDP;unicast;mode=record;server_port={};control_port={};timing_port={}", ports.data_port, ports.control_port, ports.timing_port)).expect("numeric ports"));
            outcome
                .response
                .headers
                .push(Header::new("Session", session.clone()).expect("generated session"));
            outcome.install_classic_session = Some(session);
            outcome.install_media_permit = Some(permit);
            outcome.start_media = Some(PreparedMedia::Classic { stream_id, media });
            outcome
        }
        "RECORD" | "FLUSH" => {
            if !matches!(state.media, Some(ActiveMedia::Classic { .. })) {
                return DispatchOutcome::reply(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            }
            let info = match rtp_info(request) {
                Ok(info) => info,
                Err(()) => {
                    return DispatchOutcome::reply(response_for(request, 400, "Bad Request"))
                }
            };
            let mut outcome = record_response(request);
            if request.method() == "RECORD" {
                outcome.classic_record = Some(info);
            } else {
                outcome.flush_media = Some(FlushRequest::Classic { info });
            }
            outcome
        }
        "SET_PARAMETER" => set_parameter_response(request),
        "GET_PARAMETER" if request.body().is_empty() => {
            DispatchOutcome::reply(response_for(request, 200, "OK"))
        }
        "GET_PARAMETER" => match config.current_receiver_volume() {
            Ok(volume) => get_parameter_response(request, volume),
            Err(_) => DispatchOutcome::reply(response_for(request, 503, "Service Unavailable")),
        },
        "TEARDOWN" => {
            let mut outcome = DispatchOutcome::close(response_for(request, 200, "OK"));
            outcome.teardown_media = true;
            outcome
        }
        _ => DispatchOutcome::reply(response_for(request, 501, "Not Implemented")),
    }
}

fn rtp_info(request: &Request) -> Result<ClassicRtpInfo, ()> {
    let Some(value) = unique_header(request, "RTP-Info", 1024)? else {
        return Ok(ClassicRtpInfo::default());
    };
    let text = std::str::from_utf8(value).map_err(|_| ())?;
    if !text.is_ascii() || text.bytes().any(|b| b.is_ascii_control()) {
        return Err(());
    }
    let mut result = ClassicRtpInfo::default();
    for field in text.split(';') {
        let (name, value) = field.trim().split_once('=').ok_or(())?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        match name {
            "seq" if result.sequence.is_none() => {
                result.sequence = Some(value.parse().map_err(|_| ())?)
            }
            "rtptime" if result.timestamp.is_none() => {
                result.timestamp = Some(value.parse().map_err(|_| ())?)
            }
            _ => return Err(()),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use md5::{Digest as _, Md5};
    use std::net::Ipv4Addr;
    use std::sync::atomic::AtomicUsize;

    struct TestKey(Arc<AtomicUsize>);
    impl ClassicKeyProvider for TestKey {
        fn unwrap_key(
            &self,
            _: &[u8],
        ) -> Result<Zeroizing<[u8; 16]>, super::super::super::airport_express::ClassicKeyError>
        {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Zeroizing::new([1; 16]))
        }
    }

    fn request(method: &str, target: &str, headers: &str, body: &[u8]) -> Request {
        let mut wire = format!(
            "{method} {target} RTSP/1.0\r\nCSeq: 1\r\n{headers}Content-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(body);
        let mut parser = RequestDecoder::new(RequestLimits::default()).unwrap();
        parser.feed(&wire).unwrap();
        parser
            .next_request(|_, _, _| Some(MAX_CONTROL_BODY_BYTES))
            .unwrap()
            .unwrap()
    }

    fn state() -> ConnectionState {
        let (tx, _) = watch::channel(None);
        ConnectionState::new(Ipv4Addr::LOCALHOST.into(), tx).unwrap()
    }

    fn sdp() -> Vec<u8> {
        format!("v=0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\na=rsaaeskey:{}\r\na=aesiv:{}\r\n", STANDARD.encode([0x5a;256]), STANDARD.encode([2;16])).into_bytes()
    }

    async fn call(
        request: &Request,
        config: &ServerConfig,
        state: &mut ConnectionState,
    ) -> DispatchOutcome {
        dispatch(
            request,
            ([127, 0, 0, 1], 51000).into(),
            ([127, 0, 0, 1], 47810).into(),
            config,
            &PairSetupLimiter::new(),
            &AtomicU64::new(1),
            state,
        )
        .await
    }

    #[tokio::test]
    async fn classic_challenges_before_decryption_and_commits_announcement_only_after_reply() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut config = super::super::tests::config();
        config.password = Some("secret".into());
        config.classic_key = Arc::new(TestKey(calls.clone()));
        let mut state = state();
        let denied = call(
            &request(
                "ANNOUNCE",
                "/stream",
                "Content-Type: application/sdp\r\n",
                &sdp(),
            ),
            &config,
            &mut state,
        )
        .await;
        assert_eq!(denied.response.status, 401);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(state.classic.announced.is_none());
        let challenge = state.classic.challenge.as_ref().unwrap().header_value();
        let nonce = challenge
            .split("nonce=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let ha1 = format!("{:x}", Md5::digest(b"sender:raop:secret"));
        let ha2 = format!("{:x}", Md5::digest(b"ANNOUNCE:/stream"));
        let digest = format!("{:x}", Md5::digest(format!("{ha1}:{nonce}:{ha2}")));
        let headers = format!("Content-Type: application/sdp\r\nAuthorization: Digest username=\"sender\", realm=\"raop\", nonce=\"{nonce}\", uri=\"/stream\", response=\"{digest}\"\r\n");
        let accepted = call(
            &request("ANNOUNCE", "/stream", &headers, &sdp()),
            &config,
            &mut state,
        )
        .await;
        assert_eq!(accepted.response.status, 200);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(accepted.install_classic_announcement.is_some());
        assert!(
            state.classic.announced.is_none(),
            "uncommitted response must not install the announcement"
        );
        assert!(state.classic.authenticated);
    }

    #[tokio::test]
    async fn protocol_selection_cannot_switch_after_pairing_or_announce() {
        let config = super::super::tests::config();
        let mut ap2 = state();
        assert_eq!(
            call(
                &request("POST", "/pair-pin-start", "", b""),
                &config,
                &mut ap2
            )
            .await
            .response
            .status,
            200
        );
        assert_eq!(ap2.protocol, ConnectionProtocol::AirPlay2);
        assert_eq!(
            call(
                &request(
                    "ANNOUNCE",
                    "/stream",
                    "Content-Type: application/sdp\r\n",
                    &sdp()
                ),
                &config,
                &mut ap2
            )
            .await
            .response
            .status,
            455
        );
        let mut classic = state();
        let _ = call(
            &request(
                "ANNOUNCE",
                "/stream",
                "Content-Type: application/sdp\r\n",
                b"bad",
            ),
            &config,
            &mut classic,
        )
        .await;
        assert_eq!(classic.protocol, ConnectionProtocol::Classic);
        assert_eq!(
            call(
                &request("POST", "/pair-pin-start", "", b""),
                &config,
                &mut classic
            )
            .await
            .response
            .status,
            455
        );
    }

    #[tokio::test]
    async fn classic_setup_respects_the_shared_media_permit_and_state_order() {
        let mut config = super::super::tests::config();
        config.classic_key = Arc::new(TestKey(Arc::new(AtomicUsize::new(0))));
        let mut state = state();
        state.protocol = ConnectionProtocol::Classic;
        let setup = request(
            "SETUP",
            "/stream",
            "Transport: RTP/AVP/UDP;unicast;mode=record;control_port=51001;timing_port=51002\r\n",
            b"",
        );
        assert_eq!(call(&setup, &config, &mut state).await.response.status, 455);
        let mut announced = call(
            &request(
                "ANNOUNCE",
                "/stream",
                "Content-Type: application/sdp\r\n",
                &sdp(),
            ),
            &config,
            &mut state,
        )
        .await;
        state.classic.announced = announced.install_classic_announcement.take();
        let _busy = config.media_permits.clone().try_acquire_owned().unwrap();
        assert_eq!(call(&setup, &config, &mut state).await.response.status, 453);
        assert!(state.classic.announced.is_some());
        assert!(state.media.is_none());
    }

    #[tokio::test]
    async fn invalid_session_and_unencrypted_announce_fail_closed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut config = super::super::tests::config();
        config.classic_key = Arc::new(TestKey(calls.clone()));
        let mut state = state();
        assert_eq!(
            call(
                &request(
                    "ANNOUNCE",
                    "/stream",
                    "Content-Type: application/sdp\r\n",
                    b"v=0\r\nm=audio 0 RTP/AVP 96\r\n"
                ),
                &config,
                &mut state
            )
            .await
            .response
            .status,
            400
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        state.classic.session = Some("live".into());
        assert_eq!(
            call(
                &request("GET_PARAMETER", "/stream", "Session: other\r\n", b""),
                &config,
                &mut state
            )
            .await
            .response
            .status,
            454
        );
    }

    #[test]
    fn rtp_info_is_bounded_and_rejects_duplicates_or_narrowing() {
        let valid = request(
            "RECORD",
            "/stream",
            "RTP-Info: seq=65535;rtptime=4294967295\r\n",
            b"",
        );
        assert_eq!(
            rtp_info(&valid),
            Ok(ClassicRtpInfo {
                sequence: Some(u16::MAX),
                timestamp: Some(u32::MAX)
            })
        );
        for fields in [
            "seq=65536",
            "seq=-1",
            "seq=1;seq=2",
            "rtptime=4294967296",
            "seq=1,seq=2",
        ] {
            assert!(rtp_info(&request(
                "RECORD",
                "/stream",
                &format!("RTP-Info: {fields}\r\n"),
                b""
            ))
            .is_err());
        }
        assert_eq!(
            max_body_for_request("ANNOUNCE", "/stream", &[], false),
            Some(super::super::super::classic::sdp::MAX_SDP_BYTES)
        );
    }
}
