pub fn compose(
    blocks: Vec<(u32, String)>,
    camera: Option<&str>,
    aspect_ratio: Option<&str>,
) -> String {
    let mut sorted = blocks;
    sorted.sort_by_key(|(order, _)| *order);

    let mut prompt = String::new();
    if let Some(cam) = camera {
        prompt.push_str(&format!("Camera: {cam}.\n\n"));
    }
    if let Some(ar) = aspect_ratio {
        prompt.push_str(&format!("Aspect ratio: {ar}.\n\n"));
    }
    for (_, block) in sorted {
        prompt.push_str(&block);
        prompt.push_str("\n\n");
    }
    prompt.trim_end().to_string()
}
