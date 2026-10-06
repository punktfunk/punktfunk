use pf_seats::model::SeatId;

#[test]
fn seat_ids_are_validated_during_json_decode() {
    assert!(serde_json::from_str::<SeatId>(r#""ABC""#).is_err());
}
