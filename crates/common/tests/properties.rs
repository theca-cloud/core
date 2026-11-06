//! Properties of the codecs every client stands on, over random inputs.

use proptest::prelude::*;
use theca_common::chunking::{MAX_SIZE, chunks};
use theca_common::compression::{Compression, CompressionPolicy, Mode, compress, decompress};
use theca_common::name::Name;
use unicode_normalization::UnicodeNormalization;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The chunks of any input join back into it, none over the maximum, cut
    /// the same way every time (dedup relies on it).
    #[test]
    fn chunks_join_back(seed in any::<u64>(), len in 0usize..3_000_000) {
        let mut x = seed | 1;
        let data: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 56) as u8
            })
            .collect();
        let cut = || chunks(&data[..]).map(|c| c.unwrap()).collect::<Vec<_>>();
        let first = cut();
        prop_assert!(first.iter().all(|c| c.data.len() <= MAX_SIZE));
        prop_assert_eq!(first.iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(), data.clone());
        prop_assert_eq!(first, cut());
    }

    /// What is compressed decompresses to itself, within its size.
    #[test]
    fn compression_round_trips(data in prop::collection::vec(any::<u8>(), 0..20_000), repeat in 1usize..50) {
        let data: Vec<u8> = data.repeat(repeat);
        let policy = CompressionPolicy { mode: Mode::Always, ..CompressionPolicy::default() };
        let (algorithm, stored) = compress(&policy, &data).unwrap();
        prop_assert!(stored.len() <= data.len().max(1));
        prop_assert_eq!(decompress(algorithm, &stored, data.len()).unwrap(), data.clone());
        if algorithm == Compression::Lz4 && !data.is_empty() {
            prop_assert!(decompress(algorithm, &stored, data.len() - 1).is_err());
        }
    }

    /// A name `Name::new` accepts can't step out of its folder on any disk,
    /// and is already what it normalizes to.
    #[test]
    fn accepted_names_stay_in_their_folder(raw in "\\PC{0,12}|\\.{1,3}|[a./\\\\:]{0,4}") {
        if let Ok(name) = Name::new(&raw) {
            let s = name.as_str();
            prop_assert!(!s.contains('/') && !s.contains('\\') && s != "." && s != "..");
            prop_assert!(!s.contains('\0') && !s.contains(':'));
            prop_assert_eq!(s.nfc().collect::<String>(), s);
            let again = Name::new(s).unwrap();
            prop_assert_eq!(again.as_str(), s);
        }
    }
}
