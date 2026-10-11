use super::*;

#[test]
fn file_url_uses_location_path_and_hash() {
    let resources = r#"<resources>
        <location id="cfg" path="xml/config/"/>
        <file id="maps-config" location="cfg" name="maps-config" type="xml" hash="abc123"/>
        <file id="spacemap-config" location="cfg" name="spacemap-config" type="xml"/>
    </resources>"#;
    assert_eq!(
        file_url(resources, "https://cdn.example/spacemap/", "maps-config").as_deref(),
        Some("https://cdn.example/spacemap/xml/config/maps-config.xml?__cv=abc123")
    );
    assert_eq!(
        file_url(
            resources,
            "https://cdn.example/spacemap/",
            "spacemap-config"
        )
        .as_deref(),
        Some("https://cdn.example/spacemap/xml/config/spacemap-config.xml")
    );
    assert!(file_url(resources, "https://cdn.example/spacemap/", "missing").is_none());
}

#[test]
fn resource_base_normalizes_cdn_forms() {
    let session = |cdn: &str| {
        let mut s = Session::new("sid", "tr3");
        s.flashvars.insert("cdn".into(), cdn.into());
        s.flashvars.insert("basePath".into(), "spacemap/".into());
        s
    };
    assert_eq!(
        resource_base(&session("//cdn1.bpsecure.com/")),
        "https://cdn1.bpsecure.com/spacemap/"
    );
    assert_eq!(
        resource_base(&session("cdn1.bpsecure.com")),
        "https://cdn1.bpsecure.com/spacemap/"
    );
    // No cdn flashvar falls back to the instance origin.
    let mut bare = Session::new("sid", "tr3");
    bare.flashvars.insert("basePath".into(), "spacemap/".into());
    assert_eq!(resource_base(&bare), "https://tr3.darkorbit.com/spacemap/");
}
