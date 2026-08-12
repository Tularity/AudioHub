//! `_raop._tcp` advertisement via the system Avahi daemon's D-Bus API.
//!
//! We register the service directly over D-Bus rather than shelling out to
//! `avahi-publish-service`: no dependency on `avahi-utils`, and the
//! registration is tied to our own connection, so it's published for exactly
//! as long as the [`Advertisement`] (and thus the receiver) lives.

use log::info;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use crate::Config;

const AVAHI_DEST: &str = "org.freedesktop.Avahi";
const SERVER_IFACE: &str = "org.freedesktop.Avahi.Server";
const GROUP_IFACE: &str = "org.freedesktop.Avahi.EntryGroup";

// Avahi "unspecified" sentinels: register on every interface and protocol.
const IF_UNSPEC: i32 = -1;
const PROTO_UNSPEC: i32 = -1;

/// TXT records matching what shairport-sync advertises in classic
/// (AirPlay 1) mode: ALAC or PCM (`cn`), optional RSA encryption (`et`),
/// 44100/16/2 audio. `pw` advertises whether the receiver requires a
/// password to stream: `pw=true` when `password` is `Some`, `pw=false`
/// otherwise (the boolean shairport-sync and real senders expect — not a
/// numeric). The password itself is never part of the advertisement.
pub fn txt_records(password: Option<&str>) -> Vec<String> {
    let pw = if password.is_some() {
        "pw=true"
    } else {
        "pw=false"
    };
    [
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "ek=1",
        "et=0,1",
        "sv=false",
        "da=true",
        "sr=44100",
        "ss=16",
        pw,
        "vn=65537",
        "tp=UDP",
        "md=0,1,2",
        "am=OpenAirPlay",
        "vs=105.1",
        "sf=0x4",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// A live Avahi registration. Dropping it drops the D-Bus connection, which
/// makes the daemon withdraw the service.
pub struct Advertisement {
    _connection: Connection,
    _group: OwnedObjectPath,
}

/// Register `<MAC>@<Name>` as `_raop._tcp` on `config.port` via the Avahi
/// daemon over the system D-Bus.
pub async fn publish(config: &Config) -> zbus::Result<Advertisement> {
    let service_name = config.service_name();
    let connection = Connection::system().await?;

    // A new entry group holds our service until we (or our connection) go away.
    let group: OwnedObjectPath = connection
        .call_method(
            Some(AVAHI_DEST),
            "/",
            Some(SERVER_IFACE),
            "EntryGroupNew",
            &(),
        )
        .await?
        .body()
        .deserialize()?;

    // TXT records are an array of byte arrays (`aay`) in the Avahi API.
    let txt: Vec<Vec<u8>> = txt_records(config.password.as_deref())
        .into_iter()
        .map(String::into_bytes)
        .collect();

    // AddService(interface, protocol, flags, name, type, domain, host, port, txt).
    // Empty domain/host let Avahi use the local defaults (e.g. bee.local).
    connection
        .call_method(
            Some(AVAHI_DEST),
            &group,
            Some(GROUP_IFACE),
            "AddService",
            &(
                IF_UNSPEC,
                PROTO_UNSPEC,
                0u32,
                service_name.as_str(),
                "_raop._tcp",
                "",
                "",
                config.port,
                txt,
            ),
        )
        .await?;

    connection
        .call_method(Some(AVAHI_DEST), &group, Some(GROUP_IFACE), "Commit", &())
        .await?;

    if let Ok(version) = server_version(&connection).await {
        info!(
            "advertising \"{service_name}\" on _raop._tcp port {} via {version}",
            config.port
        );
    } else {
        info!(
            "advertising \"{service_name}\" on _raop._tcp port {}",
            config.port
        );
    }

    Ok(Advertisement {
        _connection: connection,
        _group: group,
    })
}

async fn server_version(connection: &Connection) -> zbus::Result<String> {
    connection
        .call_method(
            Some(AVAHI_DEST),
            "/",
            Some(SERVER_IFACE),
            "GetVersionString",
            &(),
        )
        .await?
        .body()
        .deserialize()
}
