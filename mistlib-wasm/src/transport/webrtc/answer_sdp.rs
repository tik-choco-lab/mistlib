use std::collections::HashSet;

/// Compare every negotiated line, excluding only ICE candidate additions.
/// str::lines normalizes CRLF/LF without trimming any other SDP content.
pub(crate) fn same_negotiation(applied: &str, incoming: &str) -> bool {
    fn negotiated(line: &&str) -> bool {
        !line.starts_with("a=candidate:") && *line != "a=end-of-candidates"
    }
    applied
        .lines()
        .filter(negotiated)
        .eq(incoming.lines().filter(negotiated))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Candidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: u16,
}

/// Candidates must belong to an m-section. Duplicate/multiple MIDs are
/// ambiguous; skip those sections and leave recovery to ordinary trickle.
pub(crate) fn candidates(sdp: &str) -> Vec<Candidate> {
    let mut sections: Vec<(Vec<&str>, Vec<&str>)> = Vec::new();
    for line in sdp.lines() {
        if line.starts_with("m=") {
            sections.push((Vec::new(), Vec::new()));
        } else if let Some((mids, candidates)) = sections.last_mut() {
            if let Some(mid) = line.strip_prefix("a=mid:") {
                mids.push(mid);
            } else if let Some(candidate) = line.strip_prefix("a=candidate:") {
                candidates.push(candidate);
            }
        }
    }
    let mut result = Vec::new();
    for (index, (mids, lines)) in sections.iter().enumerate() {
        let Ok(index) = u16::try_from(index) else {
            continue;
        };
        let mid = match mids.as_slice() {
            [] => None, // The m-line index alone is unambiguous.
            [mid]
                if !mid.is_empty()
                    && sections
                        .iter()
                        .filter(|(mids, _)| mids.contains(mid))
                        .count()
                        == 1 =>
            {
                Some((*mid).to_string())
            }
            _ => continue,
        };
        for line in lines {
            result.push(Candidate {
                candidate: format!("candidate:{line}"),
                sdp_mid: mid.clone(),
                sdp_m_line_index: index,
            });
        }
    }
    result
}

/// Also suppress repeated candidate lines inside the incoming description.
pub(crate) fn new_candidates(applied: &str, incoming: &str) -> Vec<Candidate> {
    let mut seen: HashSet<_> = candidates(applied).into_iter().collect();
    candidates(incoming)
        .into_iter()
        .filter(|candidate| seen.insert(candidate.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDP: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=ice-ufrag:abc\r\na=ice-pwd:def\r\na=fingerprint:sha-256 AB\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:audio\r\na=sendrecv\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:data\r\n";

    #[test]
    fn only_candidate_lines_and_line_endings_are_ignored() {
        let incoming = format!(
            "{SDP}a=candidate:1 1 UDP 1 127.0.0.1 12345 typ host\r\na=end-of-candidates\r\n"
        );
        assert!(same_negotiation(SDP, &incoming.replace("\r\n", "\n")));
        for field in [
            "o=- 1 2",
            "s=-",
            "a=ice-ufrag:",
            "a=ice-pwd:",
            "a=fingerprint:",
            "m=audio",
            "a=sendrecv",
            "a=mid:data",
        ] {
            assert!(
                !same_negotiation(SDP, &incoming.replace(field, &format!("{field}changed"))),
                "{field}"
            );
        }
        assert!(!same_negotiation(
            SDP,
            &format!("{incoming}a=end-of-candidates:extra\r\n")
        ));
    }

    #[test]
    fn candidate_association_and_deduplication() {
        // Candidates preceding their MID still belong to the same section.
        let incoming = SDP.replace(
            "a=mid:audio",
            "a=candidate:1 1 UDP 1 127.0.0.1 12345 typ host\r\na=mid:audio",
        );
        let incoming = format!("{incoming}a=candidate:2 1 UDP 1 127.0.0.1 12346 typ host\r\na=candidate:2 1 UDP 1 127.0.0.1 12346 typ host\r\n");
        let added = new_candidates(SDP, &incoming);
        assert_eq!(added.len(), 2);
        assert_eq!(
            (added[0].sdp_mid.as_deref(), added[0].sdp_m_line_index),
            (Some("audio"), 0)
        );
        assert_eq!(
            (added[1].sdp_mid.as_deref(), added[1].sdp_m_line_index),
            (Some("data"), 1)
        );
        assert!(new_candidates(&incoming, &incoming).is_empty());
        assert!(candidates(&incoming.replace("a=mid:data", "a=mid:audio")).is_empty());
        assert!(
            candidates(&incoming.replace("a=mid:data", "a=mid:data\r\na=mid:other"))
                .iter()
                .all(|candidate| candidate.sdp_m_line_index == 0)
        );
        assert!(candidates("a=candidate:1 1 UDP 1 127.0.0.1 12345 typ host\r\n").is_empty());
    }
}
