//! Just enough XML to read S3 API responses and write the few request bodies
//! the app sends. S3 responses are flat and regular, so a tag scanner is all
//! that is needed; this is not a general-purpose parser.

/// Returns the raw inner content of every `<tag>` element, in document order.
/// Elements of the same name are assumed not to nest.
pub fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut found = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find(&open) {
        let after_name = &rest[start + open.len()..];
        // Make sure this is `<tag>` and not a longer name such as `<tagFoo>`.
        match after_name.as_bytes().first() {
            Some(b'>' | b' ' | b'/' | b'\t' | b'\n' | b'\r') => {}
            _ => {
                rest = after_name;
                continue;
            }
        }
        let Some(tag_end) = after_name.find('>') else {
            break;
        };
        if after_name[..tag_end].ends_with('/') {
            found.push("");
            rest = &after_name[tag_end + 1..];
            continue;
        }
        let body = &after_name[tag_end + 1..];
        let Some(end) = body.find(&close) else {
            break;
        };
        found.push(&body[..end]);
        rest = &body[end + close.len()..];
    }
    found
}

/// Returns the raw inner content of the first `<tag>` element.
pub fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    elements(xml, tag).into_iter().next()
}

/// Returns the unescaped text of the first `<tag>` element.
pub fn text(xml: &str, tag: &str) -> Option<String> {
    element(xml, tag).map(unescape)
}

pub fn unescape(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .map(|hex| u32::from_str_radix(hex, 16))
                .or_else(|| entity.strip_prefix('#').map(|dec| dec.parse::<u32>()))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>media</Name><Prefix>photos/</Prefix><KeyCount>2</KeyCount>
  <IsTruncated>true</IsTruncated><NextContinuationToken>abc==</NextContinuationToken>
  <Contents><Key>photos/a&amp;b.png</Key><Size>12</Size><ETag>&quot;e1&quot;</ETag></Contents>
  <Contents><Key>photos/c.png</Key><Size>34</Size><Owner><ID>x</ID></Owner></Contents>
  <CommonPrefixes><Prefix>photos/2024/</Prefix></CommonPrefixes>
  <Empty/>
</ListBucketResult>"#;

    #[test]
    fn reads_repeated_elements() {
        let contents = elements(LISTING, "Contents");
        assert_eq!(contents.len(), 2);
        assert_eq!(text(contents[0], "Key").as_deref(), Some("photos/a&b.png"));
        assert_eq!(text(contents[0], "ETag").as_deref(), Some("\"e1\""));
        assert_eq!(text(contents[1], "Size").as_deref(), Some("34"));
    }

    #[test]
    fn does_not_confuse_tags_sharing_a_prefix() {
        assert_eq!(text(LISTING, "Key").as_deref(), Some("photos/a&b.png"));
        assert_eq!(text(LISTING, "KeyCount").as_deref(), Some("2"));
        assert_eq!(text(LISTING, "Name").as_deref(), Some("media"));
    }

    #[test]
    fn reads_root_with_attributes_and_self_closing_tags() {
        assert!(element(LISTING, "ListBucketResult").is_some());
        assert_eq!(element(LISTING, "Empty"), Some(""));
        assert_eq!(element(LISTING, "Missing"), None);
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(
            unescape("a &lt;b&gt; &amp; &#39;c&#x27; &quot;"),
            "a <b> & 'c' \""
        );
        assert_eq!(unescape("fish & chips &bogus; &"), "fish & chips &bogus; &");
        assert_eq!(
            unescape(&escape("<a href=\"x\">'&'</a>")),
            "<a href=\"x\">'&'</a>"
        );
    }
}
