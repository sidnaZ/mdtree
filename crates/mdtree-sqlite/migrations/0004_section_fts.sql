CREATE VIRTUAL TABLE section_fts USING fts5(
    section_id UNINDEXED,
    node_id UNINDEXED,
    title,
    aliases,
    breadcrumb,
    summary,
    heading,
    content,
    tags,
    keywords,
    ancestor_context,
    child_context,
    tokenize = 'unicode61 remove_diacritics 2'
);
