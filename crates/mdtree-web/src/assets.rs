//! Embedded static frontend shell.
//!
//! Assets are compiled into the binary via `include_str!` so `browse-ui`
//! never requires a separate frontend development server or network access.
//! There are only three small hand-written files (no bundler), so plain
//! `include_str!` is simpler than pulling in an asset-embedding crate.

use axum::http::header;
use axum::response::IntoResponse;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const STYLE_CSS: &str = include_str!("../assets/style.css");
// EasyMDE (https://github.com/Ionaru/easy-markdown-editor), vendored rather
// than loaded from a CDN so `browse-ui` keeps working with no network
// access, matching every other asset here.
const EASYMDE_JS: &str = include_str!("../assets/vendor/easymde.min.js");
const EASYMDE_CSS: &str = include_str!("../assets/vendor/easymde.min.css");

pub(crate) async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

pub(crate) async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

pub(crate) async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
}

pub(crate) async fn easymde_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        EASYMDE_JS,
    )
}

pub(crate) async fn easymde_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        EASYMDE_CSS,
    )
}

#[cfg(test)]
mod tests {
    use super::{APP_JS, INDEX_HTML, STYLE_CSS};

    #[test]
    fn keyboard_shortcuts_toggle_markdown_and_copy_the_canonical_path_with_feedback() {
        assert!(APP_JS.contains("event.key === \"v\" || event.key === \"V\""));
        assert!(APP_JS.contains("if (event.ctrlKey || event.metaKey)"));
        assert!(APP_JS.contains("copySelectedPath().catch(reportError)"));
        assert!(APP_JS.contains("summaryForNode(selectedId)?.path"));
        assert!(APP_JS.contains("flashCopiedNode(selectedId)"));
        assert!(STYLE_CSS.contains(".node-card.path-copied:after"));
        assert!(STYLE_CSS.contains("node-path-copied"));
        assert!(INDEX_HTML.contains("Show/hide the Markdown preview pane"));
        assert!(INDEX_HTML.contains("Copy selected node path"));
    }

    #[test]
    fn a_plain_pointer_click_selects_without_opening_markdown() {
        assert!(APP_JS.contains("setSelected(finished.id);"));
        assert!(!APP_JS.contains("selectAndOpen(finished.id)"));
    }

    #[test]
    fn a_second_plain_click_on_the_same_card_within_the_window_toggles_expand() {
        assert!(APP_JS.contains("const DOUBLE_CLICK_MS = 400;"));
        assert!(APP_JS.contains(
            "lastCardClick && lastCardClick.id === finished.id && now - lastCardClick.time <= DOUBLE_CLICK_MS"
        ));
        assert!(APP_JS.contains("toggleExpand(finished.id).catch(reportError);"));
    }

    #[test]
    fn arrow_key_navigation_pans_a_newly_selected_node_back_into_the_viewport() {
        assert!(APP_JS.contains("function selectAndReveal(id) {"));
        assert!(APP_JS.contains("function panIntoViewIfNeeded(id) {"));
        assert!(APP_JS.contains("selectAndReveal(parentId);"));
        assert!(APP_JS.contains("selectAndReveal(node.children[0].id);"));
        assert!(APP_JS.contains("selectAndReveal(siblings[nextIndex].id);"));
    }

    #[test]
    fn a_node_cards_reload_control_is_reachable_by_hovering_it_not_only_when_stale() {
        assert!(STYLE_CSS.contains(".node-card:hover .node-card-reload"));
    }

    #[test]
    fn enter_toggles_expand_on_the_selected_node_like_space_does() {
        assert_eq!(
            APP_JS.matches("toggleExpand(state.selected).catch(reportError);").count(),
            2,
            "expected both the Space and Enter handlers to toggle expand on the selection"
        );
        assert!(APP_JS.contains("if (event.key === \"Enter\") {"));
        assert!(INDEX_HTML.contains("Space / Enter"));
    }

    #[test]
    fn a_node_with_more_relations_than_fit_one_row_wraps_instead_of_overflowing() {
        assert!(APP_JS.contains("relationsRow.className = \"flex flex-wrap items-center gap-1.5\";"));
        assert!(!APP_JS.contains("relationsRow.className = \"flex h-2 items-center gap-1.5\";"));
        assert!(APP_JS.contains("function relationsRowCount(node) {"));
        assert!(APP_JS.contains("const RELATIONS_DOTS_PER_ROW = Math.floor("));
    }

    #[test]
    fn arrow_right_opens_a_closed_node_with_children_instead_of_doing_nothing() {
        assert!(APP_JS.contains("else if (summaryForNode(current)?.children_count > 0) {"));
    }

    #[test]
    fn hovering_a_card_does_not_show_the_canvas_pan_grab_cursor() {
        assert!(STYLE_CSS.contains("cursor:default"));
        assert!(STYLE_CSS.contains("cursor:grabbing"));
    }

    #[test]
    fn the_reload_button_matches_the_more_actions_buttons_look_and_has_a_tooltip() {
        assert!(APP_JS.contains("reload.title = \"Reload\";"));
        assert!(APP_JS.contains("actions.appendChild(reload);"));
        assert!(APP_JS.contains("actions.appendChild(moreButton);"));
    }

    #[test]
    fn sibling_spacing_accounts_for_each_cards_actual_height_not_just_its_own() {
        assert!(APP_JS.contains("function topExtent(node) {"));
        assert!(APP_JS.contains("function bottomExtent(node) {"));
        assert!(APP_JS.contains(
            "previousY + bottomExtent(previousChild) + SLOT_GAP + topExtent(child)"
        ));
        assert!(!APP_JS.contains("function subtreeHeight(node) {"));
    }

    #[test]
    fn creating_a_child_flags_only_its_parent_stale_not_the_whole_tree() {
        assert!(APP_JS.contains("state.stale.add(pending.parentId);"));
        assert!(APP_JS.contains("suppressNextChangeSweeps: 0,"));
        assert!(APP_JS.contains("function noteSelfCausedChange() {"));
        assert!(APP_JS.contains("workspace.suppressNextChangeSweeps += 1;"));
        assert!(APP_JS.contains(
            "if (workspace.suppressNextChangeSweeps > 0) {\n        workspace.suppressNextChangeSweeps -= 1;"
        ));
    }

    #[test]
    fn search_arrow_keys_move_a_highlight_without_stealing_focus_from_the_input() {
        assert!(APP_JS.contains("} else if (event.key === \"ArrowDown\") {"));
        assert!(APP_JS.contains("moveSearchHighlight(1);"));
        assert!(APP_JS.contains("moveSearchHighlight(-1);"));
        assert!(APP_JS.contains("getSearchResultItems()[highlightedResultIndex]?.click();"));
        assert!(!APP_JS.contains(".search-result-item\")?.click();"));
        assert!(APP_JS.contains("function applySearchHighlight(index) {"));
        assert!(STYLE_CSS.contains(".search-result-item.highlighted"));
    }
}
