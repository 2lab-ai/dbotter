use crate::drivers::DriverError;
use crate::model::{
    DriverAvailability, DriverCapabilities, DriverDescriptor, DriverKind, QueryLanguage,
};

pub const REASON: &str = "MongoDB runtime is planned after the MySQL/Redis MVP";

pub const DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    kind: DriverKind::MongoDb,
    display_name: "MongoDB",
    default_port: 27017,
    availability: DriverAvailability::Planned,
    languages: &[QueryLanguage::MongoDocument],
    capabilities: DriverCapabilities::empty(),
    planned_capabilities: DriverCapabilities::CONNECT
        .union(DriverCapabilities::PING)
        .union(DriverCapabilities::DOCUMENT)
        .union(DriverCapabilities::CATALOG),
    reason: Some(REASON),
};

pub fn unavailable() -> DriverError {
    DriverError::Unavailable {
        driver: DriverKind::MongoDb,
        reason: REASON,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_is_registered_as_planned_and_document_native() {
        assert_eq!(DESCRIPTOR.availability, DriverAvailability::Planned);
        assert_eq!(DESCRIPTOR.default_port, 27017);
        assert_eq!(DESCRIPTOR.languages, &[QueryLanguage::MongoDocument]);
        assert!(matches!(
            unavailable(),
            DriverError::Unavailable {
                driver: DriverKind::MongoDb,
                ..
            }
        ));
    }
}
