use okm_core::{DocumentEncode, JunctionEncode, KeyEncode, Ref};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
struct PartyKey {
    id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PartyKey)]
#[ok_ns(2)]
struct Party {
    name: u32,
}

// The discriminator rides the 12-bit slot counter as `id << 1 | dir`, so
// 0x0000-0x07FF is the whole space; 0x0800 would land on another junction's
// slot. Refused at declaration time.
#[derive(JunctionEncode, Clone)]
#[ok_junction(0x0800)]
struct BadJunctionId {
    a: Ref<Party, PartyKey>,
    b: Ref<Party, PartyKey>,
}

fn main() {}