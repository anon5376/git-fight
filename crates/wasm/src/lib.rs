use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn golden_hash_hi() -> u32 {
    (git_fight_core::run_golden() >> 32) as u32
}

#[wasm_bindgen]
pub fn golden_hash_lo() -> u32 {
    git_fight_core::run_golden() as u32
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use git_fight_core::{run_golden, GOLDEN_HASH};
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn golden_matches_native_constant() {
        assert_eq!(run_golden(), GOLDEN_HASH);
        assert_eq!(run_golden(), run_golden());
    }
}
