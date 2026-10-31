//! Tracking products shared by the API and providers, including account-native
//! integrations and EasyPost's documented carrier products.
//!
//! https://docs.easypost.com/docs/trackers#carrier-tracking-strings

use std::{fmt, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};

macro_rules! carriers {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Serialize, JsonSchema, PartialEq, Eq, Hash)]
        pub enum Carrier {
            $(#[serde(rename = $name)] $variant),+
        }

        impl Carrier {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $name),+
                }
            }

            /// Existing SQLite identities use lowercase carrier names.
            pub fn as_db_key(&self) -> String {
                self.as_str().to_ascii_lowercase()
            }
        }
    };
}

carriers! {
    Accurate => "Accurate",
    AmazonAccount => "AmazonAccount",
    AmazonMcf => "AmazonMcf",
    AmazonMws => "AmazonMws",
    AmazonShipping => "AmazonShipping",
    Apc => "APC",
    AsendiaUsa => "AsendiaUsa",
    AustraliaPost => "AustraliaPost",
    AxlehireV3 => "AxlehireV3",
    BetterTrucks => "BetterTrucks",
    Bluestreak => "Bluestreak",
    CanadaPost => "CanadaPost",
    Canpar => "Canpar",
    ColumbusLastMile => "ColumbusLastMile",
    CourierExpress => "CourierExpress",
    CouriersPlease => "CouriersPlease",
    CsLogistics => "CSLogistics",
    DaiPost => "DaiPost",
    DeutschePostUk => "DeutschePostUK",
    DeutschePost => "DeutschePost",
    DhlEcommerceAsia => "DHLEcommerceAsia",
    DhlEcs => "DhlEcs",
    DhlExpress => "DHLExpress",
    DhlExpressDefault => "DHLExpressDefault",
    DhlPaket => "DHLPaket",
    DoorDash => "DoorDash",
    Douglas => "Douglas",
    Dpd => "DPD",
    DpdNl => "DPDNL",
    DpdUk => "DPDUK",
    EHub => "eHub",
    EPostGlobal => "EPostGlobal",
    EPostGlobalV2 => "EPostGlobalV2",
    Estafeta => "Estafeta",
    Evri => "Evri",
    FedEx => "FedEx",
    FedExCrossBorder => "FedExCrossBorder",
    FedExDefault => "FedExDefault",
    FedexSmartPost => "FedexSmartPost",
    FirstChoice => "FirstChoice",
    FirstMileConcise => "FirstMileConcise",
    Flexport => "Flexport",
    Gio => "Gio",
    Gori => "Gori",
    Gso => "GSO",
    Hailify => "Hailify",
    Henry => "Henry",
    InterlinkExpress => "InterlinkExpress",
    Jet => "Jet",
    Jitsu => "Jitsu",
    KuronekoYamato => "KuronekoYamato",
    LaserShipV2 => "LaserShipV2",
    LoomisExpress => "LoomisExpress",
    Lso => "LSO",
    Maersk => "Maersk",
    OmniParcel => "OmniParcel",
    OnTrac => "OnTrac",
    OntracV3 => "OntracV3",
    Optima => "Optima",
    OsmWorldwide => "OsmWorldwide",
    OsmWorldwideV2 => "OsmWorldwideV2",
    P2p => "P2P",
    Parcelforce => "Parcelforce",
    Parcll => "Parcll",
    PassportGlobal => "PassportGlobal",
    PostNl => "PostNL",
    Purolator => "Purolator",
    Quick => "Quick",
    Roadie => "Roadie",
    Rover => "Rover",
    RoyalMail => "RoyalMail",
    RoyalMailV3 => "RoyalMailV3",
    SfExpress => "SFExpress",
    SmartKargo => "SmartKargo",
    Sonic => "Sonic",
    SpeedX => "SpeedX",
    SpeeDee => "SpeeDee",
    Sway => "Sway",
    Swyft => "Swyft",
    Tcc => "Tcc",
    TforceConcise => "TforceConcise",
    Uds => "UDS",
    UniUni => "UniUni",
    UpsDap => "UPSDAP",
    UpsIparcel => "UPSIparcel",
    UpsMailInnovations => "UPSMailInnovations",
    Ups => "UPS",
    UsaExportPba => "UsaExportPba",
    Usps => "USPS",
    UspsShip => "UspsShip",
    Veho => "Veho",
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "Unsupported tracking carrier. Choose a supported carrier from /v1/carriers, or omit carrier for automatic detection."
)]
pub struct UnsupportedCarrier;

impl FromStr for Carrier {
    type Err = UnsupportedCarrier;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|carrier| carrier.as_str().eq_ignore_ascii_case(value.trim()))
            .ok_or(UnsupportedCarrier)
    }
}

impl fmt::Display for Carrier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for Carrier {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<'de> Deserialize<'de> for Carrier {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CarrierVisitor;

        impl de::Visitor<'_> for CarrierVisitor {
            type Value = Carrier;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a required tracking carrier from /v1/carriers")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Carrier, E> {
                Err(E::custom(
                    "A carrier is required. Choose a carrier from /v1/carriers; carrier cannot be cleared.",
                ))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Carrier, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(CarrierVisitor)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use super::{Carrier, UnsupportedCarrier};

    #[test]
    fn canonical_carriers_serialize_and_parse_without_losing_provider_identity() {
        let mut canonical = HashSet::new();
        let mut database_keys = HashSet::new();
        for carrier in Carrier::ALL {
            let encoded = serde_json::to_string(carrier).unwrap();
            let provider_name: String = serde_json::from_str(&encoded).unwrap();
            assert_eq!(provider_name, carrier.as_str());
            assert_eq!(serde_json::from_str::<Carrier>(&encoded).unwrap(), *carrier);
            assert!(canonical.insert(provider_name));
            assert!(database_keys.insert(carrier.as_db_key()));
        }
    }

    #[test]
    fn legacy_database_and_request_casing_read_back_as_the_canonical_carrier() {
        for carrier in Carrier::ALL {
            for value in [carrier.as_db_key(), carrier.as_str().to_ascii_uppercase()] {
                assert_eq!(Carrier::from_str(&value), Ok(*carrier));
                let encoded = serde_json::to_string(&value).unwrap();
                assert_eq!(serde_json::from_str::<Carrier>(&encoded).unwrap(), *carrier);
            }
        }
        assert_eq!(Carrier::from_str("  fedex  "), Ok(Carrier::FedEx));
        assert_eq!(serde_json::to_string(&Carrier::Usps).unwrap(), "\"USPS\"");
        assert_eq!(
            serde_json::to_string(&Carrier::DhlExpress).unwrap(),
            "\"DHLExpress\""
        );
    }

    #[test]
    fn unsupported_names_and_nonstring_values_are_actionable_errors() {
        for value in ["", "unrecognized-carrier", "FedExGround", "USPS/UPS"] {
            assert_eq!(Carrier::from_str(value), Err(UnsupportedCarrier));
            let encoded = serde_json::to_string(value).unwrap();
            let error = serde_json::from_str::<Carrier>(&encoded).unwrap_err();
            assert!(error.to_string().contains("Unsupported tracking carrier"));
            assert!(error.to_string().contains("/v1/carriers"));
            assert!(error.to_string().contains("automatic detection"));
        }
        for encoded in ["12", "true", "null", "{}", "[]"] {
            assert!(serde_json::from_str::<Carrier>(encoded).is_err());
        }
    }
}
