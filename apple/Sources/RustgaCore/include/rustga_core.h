#ifndef RUSTGA_CORE_H
#define RUSTGA_CORE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// A byte buffer owned by Rust. `error` is set instead of `data` on failure.
typedef struct {
    uint8_t *data;
    size_t len;
    char *error;
} RustgaBuffer;

/// JSON array of the compiled-in sources: [{ "id", "name", "lang", "browse_only" }].
/// Returns a string owned by the caller; free it with rustga_string_free.
char *rustga_sources_json(void);

/// Named homepage shelves, if the source publishes any. Same reply shape as
/// rustga_search_json, with data as [{ "title", "mangas" }].
char *rustga_sections_json(uint64_t source_id);

/// JSON reply { "ok": true, "data": [...] } or { "ok": false, "error": "..." }.
/// Blocks on the network. Returns a string owned by the caller.
char *rustga_search_json(uint64_t source_id, const char *query, int64_t page);

/// Extract recommendations from pasted text; settings JSON may be NULL.
char *rustga_discover_json(const char *thread, const char *settings);

/// Popular listing, same reply shape as rustga_search_json.
char *rustga_popular_json(uint64_t source_id, int64_t page);

/// Series details; manga_json is a Manga as returned by a listing.
char *rustga_details_json(uint64_t source_id, const char *manga_json);

/// Chapter list for a series.
char *rustga_chapters_json(uint64_t source_id, const char *manga_json);

/// Page list for one chapter. Uses the saved download when the chapter is
/// downloaded, so a saved chapter reads without the source.
char *rustga_pages_json(uint64_t source_id, const char *chapter_json);

/// One page's image bytes, read from the download when it is saved.
RustgaBuffer rustga_image(uint64_t source_id, const char *page_json);

/// Release a buffer returned by rustga_image.
void rustga_buffer_free(RustgaBuffer buffer);

/// Save chapters for offline reading on a worker thread. The reply's data is
/// the job id used by the calls below; it runs until status reports otherwise.
char *rustga_download_start_json(uint64_t source_id, const char *manga_json,
                                 const char *chapters_json);

/// Progress: { "running": bool, "message": string, "error"?: string }.
char *rustga_download_status_json(uint64_t job);

/// Stop a download at the next page boundary.
void rustga_download_cancel(uint64_t job);

/// Forget a finished job (cancels it if still running).
void rustga_download_release(uint64_t job);

/// JSON array of { "manga", "chapters" } already saved for a source.
char *rustga_downloads_json(uint64_t source_id);

/// Release a string returned by this library.
void rustga_string_free(char *pointer);

#ifdef __cplusplus
}
#endif

#endif
