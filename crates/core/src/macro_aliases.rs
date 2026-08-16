attribute_alias! {
    // Add serialize/deserialize if the serde feature is enabled. Must be applied before #[derive(...)]
    #[apply(Serde!)] = #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))];

    // Make a shader-visible struct reinterpretable as bytes for GPU upload when the gpu feature is enabled.
    #[apply(Pod!)] = #[cfg_attr(feature = "gpu", derive(bytemuck::Pod, bytemuck::Zeroable))];
}
