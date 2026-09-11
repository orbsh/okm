use okm_core::subscribe::{ChannelCell, Event, Op};
use okm_stream::{filter_field, with_previous, Stream};

#[derive(Clone, PartialEq, Debug)]
pub struct Price {
    pub symbol: String,
    pub cents: u64,
}

#[test]
fn field_filter_keeps_declared_interest_only() {
    let cell: ChannelCell<Event<String, Price>> = ChannelCell::new();
    let seen: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
    let sink = seen.clone();

    // Field-level subscription as a consumer-side combinator: this
    // consumer's interest is `cents >= 10000`, declared here, nowhere
    // else (no declaration-side field set — see PLAN).
    let downstream = Stream::new(move |ev: Event<String, Price>| {
        sink.lock().unwrap().push(ev.row.cents);
        true
    });
    let upstream = filter_field(|p: &Price| p.cents >= 10_000)(downstream);
    cell.register(upstream);

    for cents in [5_000u64, 12_345, 9_999, 20_000] {
        cell.emit(Event::new(
            Op::Put,
            1,
            "ACME".into(),
            Price { symbol: "ACME".into(), cents },
        ));
    }
    assert_eq!(*seen.lock().unwrap(), vec![12_345, 20_000]);
}

#[test]
fn with_previous_derives_before_after() {
    let cell: ChannelCell<Event<String, Price>> = ChannelCell::new();
    type SeenPair = (Option<u64>, u64);
    let seen: std::sync::Arc<std::sync::Mutex<Vec<SeenPair>>> = Default::default();
    let sink = seen.clone();

    // with_previous: the consumer-side replacement for Event.old — a
    // per-key cache in the combinator, zero write-path clone tax.
    let downstream = Stream::new(move |ev: Event<String, (Option<Price>, Price)>| {
        let (old, new) = &ev.row;
        sink.lock().unwrap().push((old.as_ref().map(|p| p.cents), new.cents));
        true
    });
    let upstream = with_previous::<String, Price>()(downstream);
    cell.register(upstream);

    cell.emit(Event::new(Op::Put, 1, "K".into(), Price { symbol: "K".into(), cents: 100 }));
    cell.emit(Event::new(Op::Put, 2, "K".into(), Price { symbol: "K".into(), cents: 150 }));
    assert_eq!(*seen.lock().unwrap(), vec![(None, 100), (Some(100), 150)]);
}
