//! The `FieldType → Arrow DataType` mapping has ONE home, and it is this crate's
//! column builder: a second restatement would export the same table as two schemas.

use okm_core::FieldType;
use okm_dynamic::arrow::arrow_type;

#[test]
fn a_uuid_exports_as_a_fixed_sixteen_byte_column() {
    assert_eq!(
        arrow_type(FieldType::Uuid7),
        arrow::datatypes::DataType::FixedSizeBinary(16),
        "a uuid is fixed width in the warehouse column too, not a variable binary blob"
    );
    assert_eq!(
        arrow_type(FieldType::Uuid4),
        arrow::datatypes::DataType::FixedSizeBinary(16)
    );
}
