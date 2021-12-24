use crate::connection;
use crate::model;
use crate::model::Identity;
use crate::model::Timestamp;
use crate::projection::Cfd;
use crate::projection::CfdAction;
use crate::projection::CfdOrder;
use crate::projection::Quote;
use crate::to_sse_event::ConnectionCloseReason::MakerVersionOutdated;
use crate::to_sse_event::ConnectionCloseReason::TakerVersionOutdated;
use bdk::bitcoin::Amount;
use rocket::request::FromParam;
use rocket::response::stream::Event;
use serde::ser::SerializeStruct;
use serde::Serialize;
use time::macros::format_description;

impl<'v> FromParam<'v> for CfdAction {
    type Error = serde_plain::Error;

    fn from_param(param: &'v str) -> Result<Self, Self::Error> {
        let action = serde_plain::from_str(param)?;
        Ok(action)
    }
}

pub trait ToSseEvent {
    fn to_sse_event(&self) -> Event;
}

impl ToSseEvent for Vec<Cfd> {
    fn to_sse_event(&self) -> Event {
        Event::json(&self).event("cfds")
    }
}

impl ToSseEvent for Vec<Identity> {
    fn to_sse_event(&self) -> Event {
        Event::json(&self).event("takers")
    }
}

impl ToSseEvent for Option<CfdOrder> {
    fn to_sse_event(&self) -> Event {
        Event::json(&self).event("order")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WalletInfo {
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
    balance: Amount,
    address: String,
    last_updated_at: Timestamp,
}

impl ToSseEvent for Option<model::WalletInfo> {
    fn to_sse_event(&self) -> Event {
        let wallet_info = self.as_ref().map(|wallet_info| WalletInfo {
            balance: wallet_info.balance,
            address: wallet_info.address.to_string(),
            last_updated_at: wallet_info.last_updated_at,
        });

        Event::json(&wallet_info).event("wallet")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionStatus {
    online: bool,
    connection_close_reason: Option<ConnectionCloseReason>,
}

#[derive(Debug, Clone, Serialize)]
pub enum ConnectionCloseReason {
    MakerVersionOutdated,
    TakerVersionOutdated,
}

impl ToSseEvent for connection::ConnectionStatus {
    fn to_sse_event(&self) -> Event {
        let connected = match self {
            connection::ConnectionStatus::Online => ConnectionStatus {
                online: true,
                connection_close_reason: None,
            },
            connection::ConnectionStatus::Offline { reason } => ConnectionStatus {
                online: false,
                connection_close_reason: reason.as_ref().map(|g| match g {
                    connection::ConnectionCloseReason::VersionMismatch {
                        maker_version,
                        taker_version,
                    } => {
                        if *maker_version < *taker_version {
                            MakerVersionOutdated
                        } else {
                            TakerVersionOutdated
                        }
                    }
                }),
            },
        };

        Event::json(&connected).event("maker_status")
    }
}

impl ToSseEvent for Option<Quote> {
    fn to_sse_event(&self) -> Event {
        Event::json(self).event("quote")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Heartbeat(time::OffsetDateTime);

impl Heartbeat {
    pub fn new() -> Self {
        Self(time::OffsetDateTime::now_utc())
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

impl serde::Serialize for Heartbeat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut heartbeat = serializer.serialize_struct("Heartbeat", 2)?;
        heartbeat.serialize_field("event", "heartbeat")?;

        let format = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
        let timestamp = self.0.format(format).expect("should always format");
        heartbeat.serialize_field("timestamp", &timestamp)?;

        heartbeat.end()
    }
}

impl ToSseEvent for Heartbeat {
    fn to_sse_event(&self) -> Event {
        Event::json(self).event("heartbeat")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_test::Token;

    #[test]
    fn heartbeat_serialization() {
        let heartbeat = Heartbeat(time::OffsetDateTime::UNIX_EPOCH);

        serde_test::assert_ser_tokens(
            &heartbeat,
            &[
                Token::Struct {
                    name: "Heartbeat",
                    len: 2,
                },
                Token::Str("event"),
                Token::Str("heartbeat"),
                Token::Str("timestamp"),
                Token::Str("1970-01-01T00:00:00"),
                Token::StructEnd,
            ],
        );
    }
}
