mod all_or_none_2214 {
    use super::*;

    /// the all-or-none choice lives in the deal's own manifest. An ordinary manifest is
    /// written exactly as before -- no new field -- and a file from before the field reads as an
    /// ordinary deal; an all-or-none one writes `"all_or_none": true` and reads it back.
    #[test]
    fn manifest_carries_all_or_none_only_when_set() {
        let ordinary = sample();
        assert!(!ordinary.all_or_none);
        let json = ordinary.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("all_or_none").is_none(), "{json}");
        assert!(!MarketManifest::from_json(&json).unwrap().all_or_none);

        let whole = MarketManifest {
            all_or_none: true,
            ..sample()
        };
        let json = whole.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["all_or_none"], true);
        assert_eq!(MarketManifest::from_json(&json).unwrap(), whole);
    }
}
