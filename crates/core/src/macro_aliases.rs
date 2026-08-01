attribute_alias! {
    // Add serialize/deserialize if the serde feature is enabled. Must be applied before #[derive(...)]
    #[apply(Serde!)] = #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))];
}
