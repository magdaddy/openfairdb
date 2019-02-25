use crate::core::{
    db::{EntryGateway, EntryIndex, EntryIndexQuery, EntryIndexer},
    entities::{AvgRatingValue, Entry},
    util::geo::{LatCoord, LngCoord},
};

use failure::Fallible;
use std::{
    ops::Bound,
    path::Path,
    sync::{Arc, Mutex},
};
use tantivy::{
    collector::TopDocs,
    query::{BooleanQuery, Occur, Query, QueryParser, RangeQuery, TermQuery},
    schema::*,
    tokenizer::{LowerCaser, RawTokenizer, Tokenizer},
    DocAddress, Document, Index, IndexWriter,
};

const OVERALL_INDEX_HEAP_SIZE_IN_BYTES: usize = 50_000_000;

struct TantivyEntryFields {
    id: Field,
    title: Field,
    description: Field,
    category: Field,
    lat: Field,
    lng: Field,
    tag: Field,
    rating: Field,
}

pub(crate) struct TantivyEntryIndex {
    fields: TantivyEntryFields,
    index: Index,
    writer: IndexWriter,
    text_query_parser: QueryParser,
}

const ID_TOKENIZER: &str = "raw";
const TAG_TOKENIZER: &str = "tag";
const TEXT_TOKENIZER: &str = "default";

fn build_schema() -> (Schema, TantivyEntryFields) {
    let id_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(ID_TOKENIZER)
                .set_index_option(IndexRecordOption::Basic),
        )
        .set_stored();
    let category_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(ID_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqs),
        )
        .set_stored();
    let tag_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TAG_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqs),
        )
        .set_stored();
    let text_options = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TEXT_TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let mut schema_builder = SchemaBuilder::default();
    let id = schema_builder.add_text_field("id", id_options);
    let lat = schema_builder.add_i64_field("lat", INT_INDEXED);
    let lng = schema_builder.add_i64_field("lng", INT_INDEXED);
    let title = schema_builder.add_text_field("title", text_options.clone());
    let description = schema_builder.add_text_field("description", text_options);
    let category = schema_builder.add_text_field("category", category_options.clone());
    let tag = schema_builder.add_text_field("tag", tag_options);
    let rating = schema_builder.add_u64_field("rating", INT_STORED | FAST);
    let schema = schema_builder.build();
    let fields = TantivyEntryFields {
        id,
        lat,
        lng,
        title,
        description,
        category,
        tag,
        rating,
    };
    (schema, fields)
}

fn register_tokenizers(index: &Index) {
    // Predefined tokenizers
    debug_assert!(index.tokenizers().get(ID_TOKENIZER).is_some());
    debug_assert!(index.tokenizers().get(TEXT_TOKENIZER).is_some());
    // Custom tokenizer(s)
    debug_assert!(index.tokenizers().get(TAG_TOKENIZER).is_none());
    index
        .tokenizers()
        .register(TAG_TOKENIZER, RawTokenizer.filter(LowerCaser));
}

fn f64_to_u64(val: f64, min: f64, max: f64) -> u64 {
    debug_assert!(val >= min);
    debug_assert!(val <= max);
    debug_assert!(min < max);
    if (val - max).abs() <= std::f64::EPSILON {
        u64::max_value()
    } else if (val - min).abs() <= std::f64::EPSILON {
        0u64
    } else {
        let norm = (val.max(min).min(max) - min) / (max - min);
        let mapped = u64::max_value() as f64 * norm;
        mapped.round() as u64
    }
}

fn u64_to_f64(val: u64, min: f64, max: f64) -> f64 {
    debug_assert!(min < max);
    if val == u64::max_value() {
        max
    } else if val == 0 {
        min
    } else {
        min + val as f64 * ((max - min) / u64::max_value() as f64)
    }
}

fn avg_rating_to_u64(avg_rating: AvgRatingValue) -> u64 {
    f64_to_u64(
        avg_rating.into(),
        AvgRatingValue::min().into(),
        AvgRatingValue::max().into(),
    )
}

fn u64_to_avg_rating(val: u64) -> AvgRatingValue {
    u64_to_f64(
        val,
        AvgRatingValue::min().into(),
        AvgRatingValue::max().into(),
    )
    .into()
}

impl TantivyEntryIndex {
    pub fn create_in_ram() -> Fallible<Self> {
        let no_path: Option<&Path> = None;
        Self::create(no_path)
    }

    pub fn create<P: AsRef<Path>>(path: Option<P>) -> Fallible<Self> {
        let (schema, fields) = build_schema();

        // TODO: Open index from existing directory
        let index = if let Some(path) = path {
            info!(
                "Creating full-text search index in directory: {}",
                path.as_ref().to_string_lossy()
            );
            Index::create_in_dir(path, schema)?
        } else {
            warn!("Creating full-text search index in RAM");
            Index::create_in_ram(schema)
        };

        register_tokenizers(&index);

        let writer = index.writer(OVERALL_INDEX_HEAP_SIZE_IN_BYTES)?;
        let text_query_parser =
            QueryParser::for_index(&index, vec![fields.title, fields.description]);
        Ok(Self {
            fields,
            index,
            writer,
            text_query_parser,
        })
    }
}

impl EntryIndexer for TantivyEntryIndex {
    fn add_or_update_entry(&mut self, entry: &Entry, avg_rating: AvgRatingValue) -> Fallible<()> {
        debug_assert!(avg_rating.is_valid());
        let id_term = Term::from_field_text(self.fields.id, &entry.id);
        self.writer.delete_term(id_term);
        let mut doc = Document::default();
        doc.add_text(self.fields.id, &entry.id);
        doc.add_i64(
            self.fields.lat,
            i64::from(LatCoord::from_deg(entry.location.lat).to_raw()),
        );
        doc.add_i64(
            self.fields.lng,
            i64::from(LngCoord::from_deg(entry.location.lng).to_raw()),
        );
        doc.add_text(self.fields.title, &entry.title);
        doc.add_text(self.fields.description, &entry.description);
        for category in &entry.categories {
            doc.add_text(self.fields.category, category);
        }
        for tag in &entry.tags {
            doc.add_text(self.fields.tag, tag);
        }
        doc.add_u64(self.fields.rating, avg_rating_to_u64(avg_rating));
        self.writer.add_document(doc);
        Ok(())
    }

    fn remove_entry_by_id(&mut self, id: &str) -> Fallible<()> {
        let id_term = Term::from_field_text(self.fields.id, id);
        self.writer.delete_term(id_term);
        Ok(())
    }

    fn flush(&mut self) -> Fallible<()> {
        self.writer.commit()?;
        self.index.load_searchers()?;
        Ok(())
    }
}

impl EntryIndex for TantivyEntryIndex {
    fn query_entries(
        &self,
        entries: &EntryGateway,
        query: &EntryIndexQuery,
        limit: usize,
    ) -> Fallible<Vec<(Entry, AvgRatingValue)>> {
        let mut sub_queries: Vec<(Occur, Box<Query>)> = Vec::with_capacity(2 + 1 + 1 + 1);

        // Bbox
        if let Some(ref bbox) = query.bbox {
            debug_assert!(bbox.is_valid());
            debug_assert!(!bbox.is_empty());
            let lat_query = RangeQuery::new_i64_bounds(
                self.fields.lat,
                Bound::Included(i64::from(bbox.south_west().lat().to_raw())),
                Bound::Included(i64::from(bbox.north_east().lat().to_raw())),
            );
            sub_queries.push((Occur::Must, Box::new(lat_query)));
            if bbox.south_west().lng() <= bbox.north_east().lng() {
                // regular (inclusive)
                let lng_query = RangeQuery::new_i64_bounds(
                    self.fields.lng,
                    Bound::Included(i64::from(bbox.south_west().lng().to_raw())),
                    Bound::Included(i64::from(bbox.north_east().lng().to_raw())),
                );
                sub_queries.push((Occur::Must, Box::new(lng_query)));
            } else {
                // inverse (exclusive)
                let lng_query = RangeQuery::new_i64_bounds(
                    self.fields.lng,
                    Bound::Excluded(i64::from(bbox.north_east().lng().to_raw())),
                    Bound::Excluded(i64::from(bbox.south_west().lng().to_raw())),
                );
                sub_queries.push((Occur::MustNot, Box::new(lng_query)));
            }
        }

        // Text
        if let Some(ref text) = query.text {
            debug_assert!(!text.trim().is_empty());
            match self.text_query_parser.parse_query(&text.to_lowercase()) {
                Ok(query) => {
                    sub_queries.push((Occur::Must, Box::new(query)));
                }
                Err(err) => {
                    warn!("Failed to parse query text '{}': {:?}", text, err);
                }
            }
        }

        // Categories
        if !query.categories.is_empty() {
            let categories_query: Box<Query> = if query.categories.len() > 1 {
                // Multiple categories
                let mut category_queries: Vec<(Occur, Box<Query>)> =
                    Vec::with_capacity(query.categories.len());
                for category in &query.categories {
                    debug_assert!(!category.trim().is_empty());
                    let category_term =
                        Term::from_field_text(self.fields.category, &category.to_lowercase());
                    let category_query = TermQuery::new(category_term, IndexRecordOption::Basic);
                    category_queries.push((Occur::Should, Box::new(category_query)));
                }
                Box::new(BooleanQuery::from(category_queries))
            } else {
                // Single category
                let category = &query.categories[0];
                debug_assert!(!category.trim().is_empty());
                let category_term =
                    Term::from_field_text(self.fields.category, &category.to_lowercase());
                Box::new(TermQuery::new(category_term, IndexRecordOption::Basic))
            };
            sub_queries.push((Occur::Must, categories_query));
        }

        // Tags
        if !query.tags.is_empty() {
            let tags_query: Box<Query> = if query.tags.len() > 1 {
                // Multiple tags
                let mut tag_queries: Vec<(Occur, Box<Query>)> =
                    Vec::with_capacity(query.categories.len());
                for tag in &query.tags {
                    debug_assert!(!tag.trim().is_empty());
                    let tag_term = Term::from_field_text(self.fields.tag, &tag.to_lowercase());
                    let tag_query = TermQuery::new(tag_term, IndexRecordOption::Basic);
                    tag_queries.push((Occur::Should, Box::new(tag_query)));
                }
                Box::new(BooleanQuery::from(tag_queries))
            } else {
                // Single tag
                let tag = &query.tags[0];
                debug_assert!(!tag.trim().is_empty());
                let tag_term = Term::from_field_text(self.fields.tag, &tag.to_lowercase());
                Box::new(TermQuery::new(tag_term, IndexRecordOption::Basic))
            };
            sub_queries.push((Occur::Must, tags_query));
        }

        let query = BooleanQuery::from(sub_queries);
        let searcher = self.index.searcher();
        // TODO (2019-02-26): Ideally we would like to order the results by
        // (score * rating) instead of only (rating). Currently Tantivy doesn't
        // support this kind of collector.
        let collector = TopDocs::with_limit(limit).order_by_field(self.fields.rating);
        let top_docs_by_rating: Vec<(u64, DocAddress)> = searcher.search(&query, &collector)?;
        let mut top_results = Vec::with_capacity(top_docs_by_rating.len());
        for (rating, doc_addr) in top_docs_by_rating {
            match searcher.doc(doc_addr) {
                Ok(doc) => {
                    if let Some(id) = doc.get_first(self.fields.id).and_then(Value::text) {
                        let categories = doc
                            .get_all(self.fields.category)
                            .into_iter()
                            .filter_map(|val| val.text().map(ToString::to_string))
                            .collect();
                        let tags = doc
                            .get_all(self.fields.tag)
                            .into_iter()
                            .filter_map(|val| val.text().map(ToString::to_string))
                            .collect();
                        match entries.get_entry_with_relations(id, categories, tags) {
                            Ok(entry) => {
                                let avg_rating = u64_to_avg_rating(rating);
                                top_results.push((entry, avg_rating));
                            }
                            Err(err) => {
                                warn!("Entry {} not found: {}", id, err);
                            }
                        }
                    } else {
                        warn!("Missing entry id in document {:?}", doc_addr);
                    }
                }
                Err(err) => {
                    warn!("Failed to load document {:?}: {}", doc_addr, err);
                }
            }
        }
        Ok(top_results)
    }
}

#[derive(Clone)]
pub struct SearchEngine(Arc<Mutex<Box<dyn EntryIndexer + Send>>>);

impl SearchEngine {
    pub fn init_in_ram() -> Fallible<SearchEngine> {
        let entry_index = TantivyEntryIndex::create_in_ram()?;
        Ok(SearchEngine(Arc::new(Mutex::new(Box::new(entry_index)))))
    }

    pub fn init_with_path<P: AsRef<Path>>(path: Option<P>) -> Fallible<SearchEngine> {
        let entry_index = TantivyEntryIndex::create(path)?;
        Ok(SearchEngine(Arc::new(Mutex::new(Box::new(entry_index)))))
    }
}

impl EntryIndex for SearchEngine {
    fn query_entries(
        &self,
        entries: &EntryGateway,
        query: &EntryIndexQuery,
        limit: usize,
    ) -> Fallible<Vec<(Entry, AvgRatingValue)>> {
        let entry_index = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        entry_index.query_entries(entries, query, limit)
    }
}

impl EntryIndexer for SearchEngine {
    fn add_or_update_entry(&mut self, entry: &Entry, avg_rating: AvgRatingValue) -> Fallible<()> {
        let mut inner = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.add_or_update_entry(entry, avg_rating)
    }

    fn remove_entry_by_id(&mut self, id: &str) -> Fallible<()> {
        let mut inner = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.remove_entry_by_id(id)
    }

    fn flush(&mut self) -> Fallible<()> {
        let mut inner = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.flush()
    }
}
