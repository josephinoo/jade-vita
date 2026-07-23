//! SDP utilities - sanitizes NVIDIA's NVST-flavored offer for the `rtc` parser, and builds
//! the NVST answer blob the signaling channel wants alongside the standard WebRTC answer.
//! Munging logic ported from OpenNOW's `native/opennow-streamer/src/sdp.rs` (regex-free).

/// ICE/DTLS credentials of one side of the session, pulled out of its SDP.
pub struct IceCredentials {
    pub ufrag: String,
    pub pwd: String,
    pub fingerprint: String,
}

pub fn extract_ice_credentials(sdp: &str) -> IceCredentials {
    let mut credentials = IceCredentials {
        ufrag: String::new(),
        pwd: String::new(),
        fingerprint: String::new(),
    };
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("a=ice-ufrag:") {
            if credentials.ufrag.is_empty() {
                credentials.ufrag = val.to_string();
            }
        } else if let Some(val) = line.strip_prefix("a=ice-pwd:") {
            if credentials.pwd.is_empty() {
                credentials.pwd = val.to_string();
            }
        } else if let Some(val) = line.strip_prefix("a=fingerprint:") {
            if credentials.fingerprint.is_empty() {
                credentials.fingerprint = val.to_string();
            }
        }
    }
    credentials
}

/// The `x-nv-sdpver` NVST parameter blob sent next to the standard answer. Its ICE/DTLS
/// credentials must be *ours* (from the answer we generated), not echoes of the server's.
pub fn build_nvst_sdp_from_answer(answer_sdp: &str) -> String {
    let ours = extract_ice_credentials(answer_sdp);
    format!(
        "v=0\r\n\
        o=SdpTest test_id_13 14 IN IPv4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        a=general.icePassword:{pwd}\r\n\
        a=general.iceUserNameFragment:{ufrag}\r\n\
        a=general.dtlsFingerprint:{fingerprint}\r\n\
        m=video 0 RTP/AVP\r\n\
        a=msid:fbc-video-0\r\n\
        a=vqos.fec.rateDropWindow:10\r\n\
        a=vqos.fec.minRequiredFecPackets:2\r\n\
        a=vqos.drc.minRequiredBitrateCheckEnabled:1\r\n\
        a=vqos.fec.repairMinPercent:5\r\n\
        a=vqos.fec.repairPercent:5\r\n\
        a=vqos.fec.repairMaxPercent:35\r\n\
        a=vqos.dynamicStreamingMode:0\r\n\
        a=vqos.drc.enable:0\r\n\
        a=vqos.dfc.enable:0\r\n\
        a=vqos.dfc.adjustResAndFps:0\r\n\
        a=video.dx9EnableNv12:1\r\n\
        a=video.dx9EnableHdr:1\r\n\
        a=vqos.qpg.enable:1\r\n\
        a=vqos.resControl.qp.qpg.featureSetting:7\r\n\
        a=bwe.useOwdCongestionControl:1\r\n\
        a=video.enableRtpNack:1\r\n\
        a=vqos.bw.txRxLag.minFeedbackTxDeltaMs:200\r\n\
        a=vqos.drc.bitrateIirFilterFactor:18\r\n\
        a=video.packetSize:1140\r\n\
        a=packetPacing.minNumPacketsPerGroup:15\r\n\
        a=vqos.bllFec.enable:0\r\n",
        pwd = ours.pwd,
        ufrag = ours.ufrag,
        fingerprint = ours.fingerprint,
    )
}

/// A literal `a.b.c.d`, or an Alliance-style host whose first DNS label encodes one
/// (`62-210-1-2.host...`). Mirrors OpenNOW's `extract_public_ip`.
pub fn extract_public_ip(host_or_ip: &str) -> Option<String> {
    if host_or_ip.is_empty() {
        return None;
    }
    if host_or_ip.parse::<std::net::Ipv4Addr>().is_ok() {
        return Some(host_or_ip.to_owned());
    }
    let first_label = host_or_ip.split('.').next().unwrap_or_default();
    let parts: Vec<&str> = first_label.split('-').collect();
    if parts.len() == 4
        && parts.iter().all(|part| {
            !part.is_empty() && part.len() <= 3 && part.as_bytes().iter().all(u8::is_ascii_digit)
        })
    {
        return Some(parts.join("."));
    }
    None
}

/// NVIDIA's offer advertises `0.0.0.0` in its connection line and candidates; substitute the
/// real server address so ICE has something to connect to. Mirrors OpenNOW's `fix_server_ip`.
pub fn fix_server_ip(sdp: &str, server_ip: &str) -> String {
    let Some(ip) = extract_public_ip(server_ip) else {
        return sdp.to_owned();
    };

    let ending = line_ending(sdp);
    sdp.split(ending)
        .map(|line| {
            if line.starts_with("c=IN IP4 0.0.0.0") {
                line.replacen("0.0.0.0", &ip, 1)
            } else if line.starts_with("a=candidate:") {
                // Candidate line layout: a=candidate:<foundation> <component> <proto> <prio>
                // <ip> <port> ... - token index 4 is the address.
                let mut parts: Vec<&str> = line.split(' ').collect();
                if parts.len() >= 6 && parts[4] == "0.0.0.0" {
                    parts[4] = &ip;
                    parts.join(" ")
                } else {
                    line.to_owned()
                }
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(ending)
}

/// NVST offers carry `a=ice-ufrag`/`a=ice-pwd`/`a=fingerprint`/`a=setup` only at session
/// level; standard WebRTC parsers expect them per media section. Copies them into each `m=`
/// block that lacks them. Mirrors OpenNOW's `duplicate_session_webrtc_attributes_to_media`.
pub fn duplicate_session_attributes_to_media(sdp: &str) -> String {
    let ending = line_ending(sdp);
    let lines: Vec<&str> = sdp.split(ending).collect();
    let Some(first_media_index) = lines.iter().position(|line| line.starts_with("m=")) else {
        return sdp.to_owned();
    };

    let is_shared_attribute = |line: &str| {
        line.starts_with("a=ice-ufrag:")
            || line.starts_with("a=ice-pwd:")
            || line.starts_with("a=fingerprint:")
            || line.starts_with("a=setup:")
    };
    let session_attributes: Vec<&str> = lines[..first_media_index]
        .iter()
        .copied()
        .filter(|line| is_shared_attribute(line))
        .collect();
    if session_attributes.is_empty() {
        return sdp.to_owned();
    }

    let mut output: Vec<String> = Vec::with_capacity(lines.len() + session_attributes.len() * 2);
    // Look ahead per media section to know if it already has its own copies.
    let section_has_attributes = |start: usize| {
        lines[start + 1..]
            .iter()
            .take_while(|line| !line.starts_with("m="))
            .any(|line| is_shared_attribute(line))
    };

    for (index, line) in lines.iter().enumerate() {
        output.push((*line).to_owned());
        if line.starts_with("m=") && !section_has_attributes(index) {
            for attribute in &session_attributes {
                output.push((*attribute).to_owned());
            }
        }
    }

    output.join(ending)
}

/// Full offer sanitation pipeline applied before handing NVIDIA's SDP to the `rtc` parser.
pub fn sanitize_offer(offer_sdp: &str, server_ip: &str) -> String {
    let fixed = fix_server_ip(offer_sdp, server_ip);
    duplicate_session_attributes_to_media(&fixed)
}

/// Payload types the offer maps to H264 (`a=rtpmap:<pt> H264/90000`).
pub fn h264_payload_types(sdp: &str) -> Vec<u8> {
    rtpmap_payload_types(sdp, "H264/")
}

/// Payload types the offer maps to Opus (`a=rtpmap:<pt> opus/48000/2`).
pub fn opus_payload_types(sdp: &str) -> Vec<u8> {
    rtpmap_payload_types(sdp, "OPUS/")
}

fn rtpmap_payload_types(sdp: &str, codec_prefix: &str) -> Vec<u8> {
    sdp.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("a=rtpmap:")?;
            let (pt, codec) = rest.split_once(' ')?;
            if codec.to_ascii_uppercase().starts_with(codec_prefix) {
                pt.parse().ok()
            } else {
                None
            }
        })
        .collect()
}

fn line_ending(sdp: &str) -> &'static str {
    if sdp.contains("\r\n") { "\r\n" } else { "\n" }
}
