import Foundation
import RustgaCore

struct SourceRow: Decodable, Identifiable, Hashable {
    let id: UInt64
    let name: String
    let lang: String
    /// The source serves listings only; open its titles on the website.
    let browseOnly: Bool

    enum CodingKeys: String, CodingKey {
        case id, name, lang
        case browseOnly = "browse_only"
    }
}

/// One named shelf from a source that publishes several homepage feeds.
struct MangaSection: Decodable, Identifiable {
    let title: String
    let mangas: [Manga]

    var id: String { title }
}

/// Mirrors the core's `Manga`. It round-trips: the core needs the whole struct
/// back for `details` and `chapters`, because a source may key on more than url.
struct Manga: Codable, Identifiable, Hashable {
    let url: String
    let title: String
    var thumbnailURL: String?
    var author: String?
    var artist: String?
    var description: String?
    var genre: String?
    var tags: [String]?
    var status: String
    var demographic: String?
    var year: Int?
    var endYear: Int?
    var altTitles: [String]

    var id: String { url }

    var genreTags: [String] {
        var seen: Set<String> = []
        let genres = (genre ?? "").split(separator: ",").map(String.init)
        return (genres + (tags ?? []))
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty && seen.insert($0.lowercased()).inserted }
    }

    enum CodingKeys: String, CodingKey {
        case url, title, author, artist, description, genre, tags, status, demographic, year
        case thumbnailURL = "thumbnail_url"
        case endYear = "end_year"
        case altTitles = "alt_titles"
    }
}

struct Chapter: Codable, Identifiable, Hashable {
    let url: String
    var name: String
    var number: Float?
    var scanlator: String?
    var uploadedAt: Int

    var id: String { url }

    // Numbered releases share completion; unnumbered entries are distinct.
    var readingKey: String { number.map { "number:\($0 == 0 ? 0 : $0)" } ?? "url:\(url)" }

    enum CodingKeys: String, CodingKey {
        case url, name, scanlator
        case number = "chapter_number"
        case uploadedAt = "uploaded_at"
    }
}

struct Page: Codable, Identifiable, Hashable {
    let index: Int
    let url: String
    var imageURL: String?

    var id: String { "\(index)-\(url)" }

    enum CodingKeys: String, CodingKey {
        case index, url
        case imageURL = "image_url"
    }
}

/// A bookmark keeps the source as well as the series so it can reopen from any tab.
struct SavedManga: Codable, Identifiable {
    let sourceID: UInt64
    let manga: Manga

    var id: String { "\(sourceID):\(manga.id)" }

    static func list(in data: Data) -> [Self] {
        (try? JSONDecoder().decode([Self].self, from: data)) ?? []
    }
}

/// Local reading position for one series, including the exact release.
struct ReadingProgress: Codable, Equatable {
    let sourceID: UInt64
    let manga: Manga
    let chapter: Chapter
    let pageIndex: Int
    let downloaded: Bool
    // Optional so histories written by earlier versions still decode.
    var completedChapters: Set<String>? = nil

    func matches(sourceID: UInt64, manga: Manga) -> Bool {
        self.sourceID == sourceID && self.manga.id == manga.id
    }

    func chapter(in chapters: [Chapter]) -> Chapter? {
        if let exact = chapters.first(where: { $0.id == chapter.id }) { return exact }
        guard let number = chapter.number else { return nil }
        return chapters.first { $0.number == number }
    }

    func page(in pages: [Page]) -> Page? {
        pages.first { $0.index == pageIndex } ?? pages.last(where: { $0.index < pageIndex }) ?? pages.first
    }
}

/// One series with the chapters the core has saved for it.
struct DownloadedManga: Decodable, Identifiable {
    let manga: Manga
    let chapters: [Chapter]

    var id: String { manga.url }
}

/// A download's last progress line; `running` goes false when it settles.
struct DownloadStatus: Decodable {
    let running: Bool
    let message: String
    let error: String?
}

private struct Reply<T: Decodable>: Decodable {
    let ok: Bool
    let data: T?
    let error: String?
}

enum CoreError: LocalizedError {
    case message(String)

    var errorDescription: String? {
        switch self {
        case .message(let text): return text
        }
    }
}

struct Recommendation: Decodable, Identifiable {
    let title: String
    let quote: String?
    let why: String?
    var id: String { title }
}

struct DiscoverAI: Encodable {
    let base_url: String
    let model: String
    let key: String
}

enum Core {
    static func discover(text: String, ai: DiscoverAI?) throws -> [Recommendation] {
        try text.withCString { thread in
            if let ai {
                return try encodeJSON(ai).withCString { try decode(rustga_discover_json(thread, $0)) }
            }
            return try decode(rustga_discover_json(thread, nil))
        }
    }

    static func sources() throws -> [SourceRow] {
        try decode(rustga_sources_json())
    }

    /// Blocks; call from a background task.
    static func popular(sourceID: UInt64, page: Int64 = 1) throws -> [Manga] {
        try decode(rustga_popular_json(sourceID, page))
    }

    /// Named homepage shelves; empty for sources that publish a single feed.
    /// Blocks; call from a background task.
    static func sections(sourceID: UInt64) throws -> [MangaSection] {
        try decode(rustga_sections_json(sourceID))
    }

    /// Blocks; call from a background task.
    static func search(sourceID: UInt64, query: String, page: Int64 = 1) throws -> [Manga] {
        try query.withCString { try decode(rustga_search_json(sourceID, $0, page)) }
    }

    static func details(sourceID: UInt64, manga: Manga) throws -> Manga {
        try withJSON(manga) { try decode(rustga_details_json(sourceID, $0)) }
    }

    static func chapters(sourceID: UInt64, manga: Manga) throws -> [Chapter] {
        try withJSON(manga) { try decode(rustga_chapters_json(sourceID, $0)) }
    }

    static func pages(sourceID: UInt64, chapter: Chapter) throws -> [Page] {
        try withJSON(chapter) { try decode(rustga_pages_json(sourceID, $0)) }
    }

    static func image(sourceID: UInt64, page: Page) throws -> Data {
        try withJSON(page) { request in
            let buffer = rustga_image(sourceID, request)
            defer { rustga_buffer_free(buffer) }
            if let error = buffer.error {
                throw CoreError.message(String(cString: error))
            }
            guard let bytes = buffer.data else {
                throw CoreError.message("the core returned no image")
            }
            return Data(bytes: bytes, count: buffer.len)
        }
    }

    /// Starts a background download in the core and returns its job id.
    /// Poll `downloadStatus` until it reports `running == false`.
    static func startDownload(sourceID: UInt64, manga: Manga, chapters: [Chapter]) throws -> UInt64 {
        let mangaJSON = try encodeJSON(manga)
        let chaptersJSON = try encodeJSON(chapters)
        return try mangaJSON.withCString { mangaPointer in
            try chaptersJSON.withCString { chaptersJSONPointer in
                try decode(
                    rustga_download_start_json(sourceID, mangaPointer, chaptersJSONPointer)
                )
            }
        }
    }

    static func downloadStatus(job: UInt64) throws -> DownloadStatus {
        try decode(rustga_download_status_json(job))
    }

    static func cancelDownload(job: UInt64) {
        rustga_download_cancel(job)
    }

    static func releaseDownload(job: UInt64) {
        rustga_download_release(job)
    }

    /// Blocking disk read; call from a background task.
    static func downloads(sourceID: UInt64) throws -> [DownloadedManga] {
        try decode(rustga_downloads_json(sourceID))
    }

    private static func decode<T: Decodable>(_ pointer: UnsafeMutablePointer<CChar>?) throws -> T {
        guard let pointer else { throw CoreError.message("the core returned nothing") }
        defer { rustga_string_free(pointer) }
        let reply = try JSONDecoder().decode(Reply<T>.self, from: Data(String(cString: pointer).utf8))
        guard reply.ok, let data = reply.data else {
            throw CoreError.message(reply.error ?? "the core reported an unknown error")
        }
        return data
    }

    private static func withJSON<T>(
        _ value: some Encodable,
        _ body: (UnsafePointer<CChar>) throws -> T
    ) throws -> T {
        try encodeJSON(value).withCString { try body($0) }
    }

    private static func encodeJSON(_ value: some Encodable) throws -> String {
        let data = try JSONEncoder().encode(value)
        guard let json = String(data: data, encoding: .utf8) else {
            throw CoreError.message("could not encode the request")
        }
        return json
    }
}
