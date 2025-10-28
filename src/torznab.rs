use quick_xml::Writer;
use quick_xml::events::{BytesCData, BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc2822};

#[derive(Debug, Clone)]
pub struct ChannelMetadata {
    pub title: String,
    pub description: String,
    pub site_link: String,
    pub api_link: String,
}

#[derive(Debug, Clone)]
pub struct TorznabItem {
    pub title: String,
    pub guid: String,
    pub guid_is_permalink: bool,
    pub link: String,
    pub comments: Option<String>,
    pub description: Option<String>,
    pub published: Option<OffsetDateTime>,
    pub size_bytes: Option<u64>,
    pub categories: Vec<TorznabCategoryRef>,
    pub attributes: Vec<TorznabAttr>,
    pub enclosure: Option<TorznabEnclosure>,
}

impl TorznabItem {
    pub fn with_default_category(mut self) -> Self {
        if self.categories.is_empty() {
            self.categories.push(TorznabCategoryRef::anime());
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct TorznabCategoryRef {
    pub id: u32,
    pub name: String,
}

impl TorznabCategoryRef {
    pub fn anime() -> Self {
        Self {
            id: ANIME_CATEGORY.id,
            name: ANIME_CATEGORY.name.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TorznabAttr {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct TorznabEnclosure {
    pub url: String,
    pub length: Option<u64>,
    pub mime_type: String,
}

#[derive(Debug, Clone)]
pub struct TorznabCategory {
    pub id: u32,
    pub name: &'static str,
    pub subcategories: &'static [TorznabSubCategory],
}

#[derive(Debug, Clone)]
pub struct TorznabSubCategory {
    pub id: u32,
    pub name: &'static str,
}

pub const ANIME_CATEGORY: TorznabCategory = TorznabCategory {
    id: 5070,
    name: "Anime",
    subcategories: &[],
};

pub fn default_categories() -> Vec<TorznabCategory> {
    vec![ANIME_CATEGORY]
}

#[derive(Debug, Error)]
pub enum TorznabBuildError {
    #[error("failed to build XML document")]
    Xml(#[from] quick_xml::Error),
    #[error("failed to format XML document as UTF-8")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("failed to format timestamp in RFC2822 format")]
    Timestamp(#[from] time::error::Format),
}

pub fn render_caps(metadata: &ChannelMetadata) -> Result<String, TorznabBuildError> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 4);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    writer.write_event(Event::Start(BytesStart::new("caps")))?;

    let mut server = BytesStart::new("server");
    server.push_attribute(("title", metadata.title.as_str()));
    server.push_attribute(("description", metadata.description.as_str()));
    server.push_attribute(("version", env!("CARGO_PKG_VERSION")));
    writer.write_event(Event::Empty(server))?;

    let mut limits = BytesStart::new("limits");
    limits.push_attribute(("default", "100"));
    limits.push_attribute(("max", "100"));
    limits.push_attribute(("min", "1"));
    writer.write_event(Event::Empty(limits))?;

    let mut registration = BytesStart::new("registration");
    registration.push_attribute(("available", "no"));
    registration.push_attribute(("open", "no"));
    writer.write_event(Event::Empty(registration))?;

    for (mode, supported) in [("search", "q"), ("tv-search", "q")] {
        let mut searching = BytesStart::new("searching");
        searching.push_attribute(("type", mode));
        searching.push_attribute(("available", "yes"));
        searching.push_attribute(("supportedParams", supported));
        writer.write_event(Event::Empty(searching))?;
    }

    writer.write_event(Event::Start(BytesStart::new("categories")))?;

    for category in default_categories() {
        let id_attr = category.id.to_string();
        let mut category_el = BytesStart::new("category");
        category_el.push_attribute(("id", id_attr.as_str()));
        category_el.push_attribute(("name", category.name));
        writer.write_event(Event::Start(category_el))?;

        for sub in category.subcategories {
            let sub_id = sub.id.to_string();
            let mut sub_el = BytesStart::new("subcat");
            sub_el.push_attribute(("id", sub_id.as_str()));
            sub_el.push_attribute(("name", sub.name));
            writer.write_event(Event::Empty(sub_el))?;
        }

        writer.write_event(Event::End(BytesEnd::new("category")))?;
    }

    writer.write_event(Event::End(BytesEnd::new("categories")))?;
    writer.write_event(Event::End(BytesEnd::new("caps")))?;

    Ok(String::from_utf8(writer.into_inner())?)
}

pub fn render_feed(
    metadata: &ChannelMetadata,
    items: &[TorznabItem],
) -> Result<String, TorznabBuildError> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    let mut rss = BytesStart::new("rss");
    rss.push_attribute(("version", "2.0"));
    rss.push_attribute(("xmlns:atom", "http://www.w3.org/2005/Atom"));
    rss.push_attribute(("xmlns:torznab", "http://torznab.com/schemas/2015/feed"));
    writer.write_event(Event::Start(rss))?;

    writer.write_event(Event::Start(BytesStart::new("channel")))?;
    write_text_element(&mut writer, "title", &metadata.title)?;
    write_text_element(&mut writer, "description", &metadata.description)?;
    write_text_element(&mut writer, "link", &metadata.site_link)?;

    let mut atom_link = BytesStart::new("atom:link");
    atom_link.push_attribute(("href", metadata.api_link.as_str()));
    atom_link.push_attribute(("rel", "self"));
    atom_link.push_attribute(("type", "application/rss+xml"));
    writer.write_event(Event::Empty(atom_link))?;

    for item in items
        .iter()
        .cloned()
        .map(TorznabItem::with_default_category)
    {
        writer.write_event(Event::Start(BytesStart::new("item")))?;
        write_text_element(&mut writer, "title", &item.title)?;

        let mut guid_el = BytesStart::new("guid");
        let is_permalink = if item.guid_is_permalink {
            "true"
        } else {
            "false"
        };
        guid_el.push_attribute(("isPermaLink", is_permalink));
        writer.write_event(Event::Start(guid_el))?;
        writer.write_event(Event::Text(BytesText::new(&item.guid)))?;
        writer.write_event(Event::End(BytesEnd::new("guid")))?;

        write_text_element(&mut writer, "link", &item.link)?;

        if let Some(comments) = item.comments.as_deref() {
            write_text_element(&mut writer, "comments", comments)?;
        }

        if let Some(description) = item.description.as_deref() {
            write_cdata_element(&mut writer, "description", description)?;
        }

        if let Some(published) = item.published {
            let formatted = published.format(&Rfc2822)?;
            write_text_element(&mut writer, "pubDate", &formatted)?;
        }

        if let Some(size) = item.size_bytes {
            write_text_element(&mut writer, "size", &size.to_string())?;
        }

        for category in item.categories {
            let mut category_el = BytesStart::new("category");
            let id_attr = category.id.to_string();
            category_el.push_attribute(("id", id_attr.as_str()));
            category_el.push_attribute(("name", category.name.as_str()));
            writer.write_event(Event::Start(category_el))?;
            writer.write_event(Event::CData(BytesCData::new(category.name.as_str())))?;
            writer.write_event(Event::End(BytesEnd::new("category")))?;
        }

        if let Some(enclosure) = item.enclosure {
            let mut enclosure_el = BytesStart::new("enclosure");
            enclosure_el.push_attribute(("url", enclosure.url.as_str()));
            enclosure_el.push_attribute(("type", enclosure.mime_type.as_str()));
            if let Some(length) = enclosure.length {
                enclosure_el.push_attribute(("length", length.to_string().as_str()));
            }
            writer.write_event(Event::Empty(enclosure_el))?;
        }

        for attr in item.attributes {
            let mut attr_el = BytesStart::new("torznab:attr");
            attr_el.push_attribute(("name", attr.name.as_str()));
            attr_el.push_attribute(("value", attr.value.as_str()));
            writer.write_event(Event::Empty(attr_el))?;
        }

        writer.write_event(Event::End(BytesEnd::new("item")))?;
    }

    writer.write_event(Event::End(BytesEnd::new("channel")))?;
    writer.write_event(Event::End(BytesEnd::new("rss")))?;

    Ok(String::from_utf8(writer.into_inner())?)
}

fn write_text_element(
    writer: &mut Writer<Vec<u8>>,
    name: &str,
    value: &str,
) -> Result<(), quick_xml::Error> {
    writer.write_event(Event::Start(BytesStart::new(name)))?;
    writer.write_event(Event::Text(BytesText::new(value)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))?;
    Ok(())
}

fn write_cdata_element(
    writer: &mut Writer<Vec<u8>>,
    name: &str,
    value: &str,
) -> Result<(), quick_xml::Error> {
    writer.write_event(Event::Start(BytesStart::new(name)))?;
    writer.write_event(Event::CData(BytesCData::new(value)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))?;
    Ok(())
}
