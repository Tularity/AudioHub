//! Minimal DMAP (DAAP-tagged) metadata walker.
//!
//! `SET_PARAMETER` with `Content-Type: application/x-dmap-tagged` carries
//! now-playing metadata as DMAP: a sequence of entries, each a 4-byte ASCII
//! tag code followed by a big-endian u32 payload length and that many bytes
//! of payload. The track fields sit inside an `mlit` (dmap.listingitem)
//! container whose payload is itself such a sequence. This module walks
//! exactly that shape — tag code plus length — and extracts the fields the
//! host can display; unknown tags are skipped silently and a truncated
//! entry ends the walk. It is not a general DAAP implementation.

/// Track metadata from one DMAP payload. Fields the payload did not carry
/// are `None` — a payload is a complete statement about the current track,
/// not a delta.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrackMetadata {
    /// `minm` (dmap.itemname)
    pub title: Option<String>,
    /// `asar` (daap.songartist)
    pub artist: Option<String>,
    /// `asal` (daap.songalbum)
    pub album: Option<String>,
}

/// Extract track metadata from a DMAP payload. Returns `None` when the body
/// contains no `mlit` container — without it there is nothing recognizable
/// as a track statement. Strings are converted lossily; malformed trailing
/// bytes never fail the parse, they just end it.
pub fn parse(body: &[u8]) -> Option<TrackMetadata> {
    let mut meta: Option<TrackMetadata> = None;
    walk(body, &mut |tag, payload| {
        if tag == *b"mlit" {
            let m = meta.get_or_insert_with(TrackMetadata::default);
            walk(payload, &mut |tag, payload| {
                let field = match &tag {
                    b"minm" => &mut m.title,
                    b"asar" => &mut m.artist,
                    b"asal" => &mut m.album,
                    _ => return,
                };
                *field = Some(String::from_utf8_lossy(payload).into_owned());
            });
        }
    });
    meta
}

/// Walk one level of DMAP entries, calling `f(tag, payload)` for each. An
/// entry whose declared length runs past the end of the data (truncation —
/// or bytes that are not DMAP at all) ends the walk.
fn walk(mut data: &[u8], f: &mut impl FnMut([u8; 4], &[u8])) {
    while data.len() >= 8 {
        let tag: [u8; 4] = data[..4].try_into().unwrap();
        let len = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
        let rest = &data[8..];
        if len > rest.len() {
            return;
        }
        f(tag, &rest[..len]);
        data = &rest[len..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut e = Vec::with_capacity(8 + payload.len());
        e.extend_from_slice(tag);
        e.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        e.extend_from_slice(payload);
        e
    }

    fn mlit(children: &[Vec<u8>]) -> Vec<u8> {
        entry(b"mlit", &children.concat())
    }

    #[test]
    fn extracts_title_artist_album_from_mlit() {
        let body = mlit(&[
            entry(b"minm", "Song Title".as_bytes()),
            entry(b"asar", "The Artist".as_bytes()),
            entry(b"asal", "The Album".as_bytes()),
        ]);
        assert_eq!(
            parse(&body),
            Some(TrackMetadata {
                title: Some("Song Title".into()),
                artist: Some("The Artist".into()),
                album: Some("The Album".into()),
            })
        );
    }

    #[test]
    fn unknown_tags_are_skipped() {
        // Real payloads interleave many tags we don't want (song time,
        // persistent id, ...) — they must not derail the wanted ones.
        let body = mlit(&[
            entry(b"astm", &180_000u32.to_be_bytes()),
            entry(b"minm", b"Song"),
            entry(b"mper", &42u64.to_be_bytes()),
            entry(b"asal", b"Album"),
        ]);
        let meta = parse(&body).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Song"));
        assert_eq!(meta.album.as_deref(), Some("Album"));
        assert_eq!(meta.artist, None);
    }

    #[test]
    fn missing_fields_stay_none() {
        // A title-only statement: artist/album are None, not carried over.
        let meta = parse(&mlit(&[entry(b"minm", b"Just A Title")])).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Just A Title"));
        assert_eq!(meta.artist, None);
        assert_eq!(meta.album, None);
    }

    #[test]
    fn no_mlit_container_yields_none() {
        // The wanted tags outside their container are not a track statement.
        assert_eq!(parse(&entry(b"minm", b"loose")), None);
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"not dmap at all"), None);
    }

    #[test]
    fn truncated_entries_end_the_walk_without_losing_earlier_fields() {
        // Truncated entry after a complete container: container survives.
        let mut body = mlit(&[entry(b"minm", b"Song")]);
        body.extend_from_slice(b"asar");
        body.extend_from_slice(&100u32.to_be_bytes()); // claims 100 bytes, has 0
        assert_eq!(parse(&body).unwrap().title.as_deref(), Some("Song"));

        // Truncated entry inside the container: earlier siblings survive.
        let mut inner = entry(b"minm", b"Song");
        inner.extend_from_slice(b"asal");
        inner.extend_from_slice(&100u32.to_be_bytes());
        let meta = parse(&entry(b"mlit", &inner)).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Song"));
        assert_eq!(meta.album, None);
    }

    #[test]
    fn invalid_utf8_is_converted_lossily() {
        let meta = parse(&mlit(&[entry(b"minm", &[0xff, b'A'])])).unwrap();
        assert_eq!(meta.title.as_deref(), Some("\u{fffd}A"));
    }
}
