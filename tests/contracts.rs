use dbotter::drivers;
use dbotter::model::{DriverAvailability, DriverCapabilities, DriverKind, QueryLanguage};

#[test]
fn all_three_driver_boundaries_are_registered() {
    let descriptors = drivers::descriptors();
    assert_eq!(descriptors.len(), 3);
    assert_eq!(descriptors[0].kind, DriverKind::MySql);
    assert_eq!(descriptors[1].kind, DriverKind::Redis);
    assert_eq!(descriptors[2].kind, DriverKind::MongoDb);
    assert_eq!(descriptors[2].availability, DriverAvailability::Planned);
    assert_eq!(descriptors[2].languages, &[QueryLanguage::MongoDocument]);
    assert!(
        descriptors[0]
            .capabilities
            .contains(DriverCapabilities::SQL)
    );
    assert!(
        descriptors[0]
            .capabilities
            .contains(DriverCapabilities::CATALOG)
    );
    assert!(
        !descriptors[0]
            .planned_capabilities
            .contains(DriverCapabilities::CATALOG)
    );
    assert!(
        descriptors[1]
            .capabilities
            .contains(DriverCapabilities::COMMAND)
    );
    assert!(
        descriptors[2]
            .planned_capabilities
            .contains(DriverCapabilities::DOCUMENT | DriverCapabilities::CATALOG)
    );
}

#[test]
fn driver_error_display_redacts_raw_backend_messages() {
    use std::error::Error as _;

    let mysql_secret = "sql contains top-secret-marker";
    let mysql = drivers::DriverError::from(sqlx::Error::Protocol(mysql_secret.to_owned()));
    assert_eq!(mysql.to_string(), "mysql operation failed");
    assert!(!mysql.to_string().contains(mysql_secret));
    assert!(!format!("{mysql:?}").contains(mysql_secret));
    assert!(mysql.source().is_some());

    let redis_secret = "redis contains top-secret-marker";
    let redis_source = redis::RedisError::from((redis::ErrorKind::Io, redis_secret));
    let redis = drivers::DriverError::from(redis_source);
    assert_eq!(redis.to_string(), "redis operation failed");
    assert!(!redis.to_string().contains(redis_secret));
    assert!(!format!("{redis:?}").contains(redis_secret));
    assert!(redis.source().is_some());

    for error in [
        drivers::DriverError::InvalidConfig {
            driver: DriverKind::Redis,
            message: "secret-ca-path-/sentinel/ca.pem".to_owned(),
        },
        drivers::DriverError::RedisParse("raw-command-sentinel".to_owned()),
        drivers::DriverError::Unsupported {
            driver: DriverKind::MySql,
            operation: "raw-export-path-/sentinel/export.csv".to_owned(),
        },
    ] {
        let debug = format!("{error:?}");
        let display = error.to_string();
        for sentinel in ["secret-ca-path", "raw-command", "raw-export-path"] {
            assert!(!debug.contains(sentinel));
            assert!(!display.contains(sentinel));
        }
    }
}

#[test]
fn driver_kind_wire_names_match_config_contract() {
    assert_eq!(
        serde_json::to_string(&DriverKind::MySql).expect("serialize"),
        "\"mysql\""
    );
    assert_eq!(
        serde_json::to_string(&DriverKind::MongoDb).expect("serialize"),
        "\"mongodb\""
    );
}
