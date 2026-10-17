//! Embedded standard-library sources consumed by the common module resolver.
pub(crate) const SOURCES: &[(&str, &str)] = &[
    ("array", include_str!("../core/array.cog")),
    ("ascii", include_str!("../core/ascii.cog")),
    ("base64", include_str!("../core/base64.cog")),
    ("clone", include_str!("../core/clone.cog")),
    ("collections", include_str!("../core/collections.cog")),
    ("eq", include_str!("../core/eq.cog")),
    ("err", include_str!("../core/err.cog")),
    ("hex", include_str!("../core/hex.cog")),
    ("io", include_str!("../core/io.cog")),
    ("math", include_str!("../core/math.cog")),
    ("ord", include_str!("../core/ord.cog")),
    ("random", include_str!("../core/random.cog")),
    ("search", include_str!("../core/search.cog")),
    ("sort", include_str!("../core/sort.cog")),
    ("str", include_str!("../core/str.cog")),
];
