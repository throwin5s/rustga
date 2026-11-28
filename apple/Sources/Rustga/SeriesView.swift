import SwiftUI
import Foundation
import CryptoKit
#if os(macOS)
import AppKit
#else
import UIKit
#endif

/// Chapter lists are stable once fetched and expensive to rebuild on every
/// series visit. Keep a small disk copy so the list appears immediately while
/// the fresh copy is loaded in the background.
actor ChapterListCache {
    static let shared = ChapterListCache()

    private let directory: URL
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init() {
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        directory = base.appendingPathComponent("rustga-chapters", isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    }

    func chapters(for key: String) -> [Chapter]? {
        let url = file(for: key)
        guard let data = try? Data(contentsOf: url) else { return nil }
        guard let chapters = try? decoder.decode([Chapter].self, from: data) else {
            try? FileManager.default.removeItem(at: url)
            return nil
        }
        return chapters
    }

    func store(_ chapters: [Chapter], for key: String) {
        guard let data = try? encoder.encode(chapters) else { return }
        try? data.write(to: file(for: key), options: .atomic)
    }

    private func file(for key: String) -> URL {
        let digest = SHA256.hash(data: Data(key.utf8)).map { String(format: "%02x", $0) }.joined()
        return directory.appendingPathComponent(digest)
    }
}

/// Series page: details from the source, then the chapter list. Each chapter can
/// be saved through the core's download worker while the page stays responsive.
struct SeriesView: View {
    let manga: Manga
    let sourceID: UInt64
    /// On macOS the cover is the shared element between the grid and this page.
    let hero: Namespace.ID?
    let progress: ReadingProgress?
    let onOpen: (Chapter, [Chapter]) -> Void
    let onBack: () -> Void

    @State private var detail: Manga?
    @State private var chapters: [Chapter] = []
    @State private var message: String?
    @State private var loading = true
    @State private var reloading = false
    /// Keeps one release per chapter number; the alternative versions stay
    /// available by turning this off.
    @AppStorage("hideDuplicateReleases") private var hideDuplicates = true
    @AppStorage("savedManga") private var savedManga = Data()
    @AppStorage("seriesCoverArtTheme") private var coverArtTheme = false
    @State private var sortAscending = false
    @State private var descriptionExpanded = false
    @State private var unreadOnly = false
    @State private var downloadedOnly = false
    @State private var loadedSavedChapters = false
    @State private var scanlatorFilter: Set<String> = []
    #if os(macOS)
    @State private var refreshing = false
    #endif

    @State private var savedChapters: Set<String> = []
    @State private var run: DownloadRun?
    @State private var downloadNote: String?
    @State private var downloadFailure: String?

    /// The chapter being saved and, once the core has started it, the job id.
    private struct DownloadRun {
        let chapterURL: String
        var job: UInt64?
        var cancelled = false
    }

    init(
        manga: Manga,
        sourceID: UInt64,
        hero: Namespace.ID? = nil,
        progress: ReadingProgress?,
        onOpen: @escaping (Chapter, [Chapter]) -> Void,
        onBack: @escaping () -> Void
    ) {
        self.manga = manga
        self.sourceID = sourceID
        self.hero = hero
        self.progress = progress
        self.onOpen = onOpen
        self.onBack = onBack
    }

    private var shown: Manga { detail ?? manga }

    private var bookmark: SavedManga { SavedManga(sourceID: sourceID, manga: manga) }

    private func toggleBookmark() {
        var entries = SavedManga.list(in: savedManga)
        if let index = entries.firstIndex(where: { $0.id == bookmark.id }) {
            entries.remove(at: index)
        } else {
            entries.insert(bookmark, at: 0)
        }
        if let data = try? JSONEncoder().encode(entries) { savedManga = data }
    }

    /// Sources can list the same chapter number from several groups. When one
    /// release is shown per number, official uploads win before scanlations.
    /// Chapters with no number are left alone because they cannot be matched
    /// reliably.
    private var visibleChapters: [Chapter] {
        let list = unreadOnly ? chapterListCandidates.filter { !hasRead($0) } : chapterListCandidates
        return (hideDuplicates ? Self.preferredReleases(list) : list).sorted(by: chapterOrder)
    }

    private var chapterListCandidates: [Chapter] {
        chapters.filter { chapter in
            (!downloadedOnly || savedChapters.contains(chapter.url)) &&
                (scanlatorFilter.isEmpty || scanlatorFilter.contains(scanlatorName(chapter) ?? ""))
        }
    }

    private func hasRead(_ chapter: Chapter) -> Bool {
        progress?.completedChapters?.contains(chapter.readingKey) == true
    }

    private var nextUnreadDownload: Chapter? {
        guard loadedSavedChapters else { return nil }
        let savedKeys = Set(chapters.lazy.filter { savedChapters.contains($0.url) }.map(\.readingKey))
        let oldestFirst = Array(Self.newestFirst(Self.preferredReleases(chapters)).reversed())
        let start = progress.flatMap { current in
            oldestFirst.firstIndex { $0.readingKey == current.chapter.readingKey }
        } ?? oldestFirst.startIndex
        return oldestFirst[start...].first { !hasRead($0) && !savedKeys.contains($0.readingKey) }
    }

    static func preferredReleases(_ chapters: [Chapter]) -> [Chapter] {
        var best: [Float: Chapter] = [:]
        var unnumbered: [Chapter] = []
        for chapter in chapters {
            guard let number = chapter.number else {
                unnumbered.append(chapter)
                continue
            }
            if let current = best[number], releaseRank(current) <= releaseRank(chapter) { continue }
            best[number] = chapter
        }
        return Array(best.values) + unnumbered
    }

    /// Releases hidden by "One release per chapter", keyed by chapter number,
    /// so each row can still offer its other versions.
    private var alternates: [Float: [Chapter]] {
        guard hideDuplicates else { return [:] }
        var groups: [Float: [Chapter]] = [:]
        for chapter in chapters {
            guard let number = chapter.number else { continue }
            if downloadedOnly && !savedChapters.contains(chapter.url) { continue }
            if unreadOnly && hasRead(chapter) { continue }
            if !scanlatorFilter.isEmpty, !scanlatorFilter.contains(scanlatorName(chapter) ?? "") { continue }
            groups[number, default: []].append(chapter)
        }
        var extras: [Float: [Chapter]] = [:]
        for (number, group) in groups where group.count > 1 {
            let primary = Self.preferredReleases(group).first
            extras[number] = group
                .filter { $0.id != primary?.id }
                .sorted {
                    (Self.releaseRank($0), scanlatorName($0) ?? "") < (Self.releaseRank($1), scanlatorName($1) ?? "")
                }
        }
        return extras
    }

    /// The reader always needs newest-first order, regardless of how the list
    /// on this page is currently sorted.
    private var readingChapters: [Chapter] {
        Self.newestFirst(hideDuplicates ? Self.preferredReleases(chapters) : chapters)
    }

    private func continueReading(_ progress: ReadingProgress) {
        var reading = readingChapters.filter { $0.id != progress.chapter.id }
        if hideDuplicates, let number = progress.chapter.number {
            reading.removeAll { $0.number == number }
        }
        onOpen(progress.chapter, Self.newestFirst(reading + [progress.chapter]))
    }

    static func newestFirst(_ list: [Chapter]) -> [Chapter] {
        list.sorted {
            let left = $0.number ?? -Float.greatestFiniteMagnitude
            let right = $1.number ?? -Float.greatestFiniteMagnitude
            if left != right { return left > right }
            return $0.url < $1.url
        }
    }

    private var scanlators: [String] {
        Array(Set(chapters.compactMap { scanlatorName($0) })).sorted()
    }

    private func scanlatorName(_ chapter: Chapter) -> String? { Self.scanlatorName(chapter) }

    private static func scanlatorName(_ chapter: Chapter) -> String? {
        let name = chapter.scanlator?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return name.isEmpty ? nil : name
    }

    private func chapterOrder(_ left: Chapter, _ right: Chapter) -> Bool {
        let leftNumber = left.number ?? -Float.greatestFiniteMagnitude
        let rightNumber = right.number ?? -Float.greatestFiniteMagnitude
        if leftNumber != rightNumber {
            return sortAscending ? leftNumber < rightNumber : leftNumber > rightNumber
        }
        return left.url < right.url
    }

    /// Official publishers and platform uploads win over scanlations when two
    /// releases carry the same chapter number. MangaDot marks its scraped
    /// official chapters as `source=scraper` and leaves the scanlator blank;
    /// user-uploaded scanlations use `source=user`.
    private static func releaseRank(_ chapter: Chapter) -> Int {
        if chapter.url.contains("source=scraper") { return 0 }
        guard let raw = scanlatorName(chapter) else { return 3 }
        let name = raw.lowercased()
        if name == "official" || name.hasPrefix("official ") || name.hasSuffix(" official") {
            return 0
        }
        if Self.officialScanlators.contains(where: { name.contains($0) }) { return 0 }
        return 1
    }

    private static let officialScanlators = [
        "webtoon", "tapas", "mangak", "manga k", "mangaplus", "manga plus",
        "comikey", "inkr", "kodansha", "viz", "yen press",
        "seven seas", "square enix", "shueisha", "naver", "manta", "lezhin",
        "toomics", "wuxiaworld",
    ]

    var body: some View {
        ZStack(alignment: .top) {
            pageBackground.ignoresSafeArea()

            if coverArtTheme, shown.thumbnailURL != nil {
                heroBackdrop
            }

            VStack(spacing: 0) {
                header

                chapterList
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(pageBackground)
        .task { await load() }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Button(action: onBack) {
                Image(systemName: "chevron.left")
                    .font(.body.weight(.semibold))
            }
            .buttonStyle(.glass)
            .accessibilityLabel("Back to results")
            .help("Back to results")

            Text(shown.title)
                .font(.headline)
                .lineLimit(1)
                .layoutPriority(1)

            Spacer(minLength: 8)

            let isSaved = SavedManga.list(in: savedManga).contains { $0.id == bookmark.id }
            Button(action: toggleBookmark) {
                Image(systemName: isSaved ? "bookmark.fill" : "bookmark")
            }
            .buttonStyle(.glass)
            .accessibilityLabel(isSaved ? "Remove from Library" : "Save to Library")
            .help(isSaved ? "Remove from Library" : "Save to Library")

            #if os(macOS)
            if refreshing {
                ProgressView()
                    .controlSize(.small)
            } else {
                Button(action: refresh) {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.glass)
                .disabled(loading)
                .accessibilityLabel("Refresh series")
                .help("Refresh details and chapters")
            }
            #endif

            if let note = downloadFailure ?? downloadNote {
                Text(note)
                    .font(.caption2)
                    .foregroundStyle(downloadFailure == nil ? Color.secondary : Color.red)
                    .lineLimit(1)
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 11)
        .background(.bar)
        .overlay(alignment: .bottom) {
            Divider().opacity(0.4)
        }
    }

    private var heroBackdrop: some View {
        CachedCover(url: shown.thumbnailURL.flatMap { URL(string: $0) })
            .frame(maxWidth: .infinity)
            .frame(height: 430)
            .scaleEffect(1.12)
            .blur(radius: 52)
            .saturation(1.45)
            .opacity(0.34)
            .clipped()
            .mask(
                LinearGradient(
                    stops: [
                        .init(color: .black, location: 0),
                        .init(color: .black.opacity(0.75), location: 0.46),
                        .init(color: .clear, location: 1)
                    ],
                    startPoint: .top,
                    endPoint: .bottom
                )
            )
            .allowsHitTesting(false)
            .ignoresSafeArea(edges: .top)
    }

    @ViewBuilder
    private var summary: some View {
        #if os(iOS)
        if coverArtTheme {
            mobileSummary
        } else {
            desktopSummary
        }
        #else
        desktopSummary
        #endif
    }

    private var desktopSummary: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .bottom, spacing: 22) {
                summaryCover

                VStack(alignment: .leading, spacing: 10) {
                    Text(shown.title)
                        .font(.system(size: 28, weight: .bold))
                        .lineLimit(3)
                        .fixedSize(horizontal: false, vertical: true)

                    if let altTitlesLine {
                        Text(altTitlesLine)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .fixedSize(horizontal: false, vertical: true)
                    }

                    metadata
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            readingAction
            tags

            if let description = shown.description, !description.isEmpty {
                about(description)
            }
        }
        .padding(.horizontal, 26)
        .padding(.top, 26)
        .padding(.bottom, 22)
    }

    private var mobileSummary: some View {
        VStack(spacing: 16) {
            summaryCover
                .padding(.top, 2)

            VStack(spacing: 9) {
                Text(shown.title)
                    .font(.title2.bold())
                    .multilineTextAlignment(.center)
                    .lineLimit(4)
                    .fixedSize(horizontal: false, vertical: true)

                if let altTitlesLine {
                    Text(altTitlesLine)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                }

                mobileMetadata
                readingAction
                tags
            }
            .frame(maxWidth: .infinity)

            if let description = shown.description, !description.isEmpty {
                about(description)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 22)
        .padding(.top, 20)
        .padding(.bottom, 20)
    }

    private var mobileMetadata: some View {
        VStack(spacing: 6) {
            if !shown.status.isEmpty && shown.status != "Unknown" {
                statusBadge
            }

            if let detailLine {
                Text(detailLine)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .lineLimit(3)
            }

            if let byline {
                Text(byline)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .lineLimit(2)
            }
        }
        .frame(maxWidth: .infinity)
    }

    @ViewBuilder
    private var summaryCover: some View {
        #if os(macOS)
        if let hero {
            coverArt
                .matchedGeometryEffect(id: manga.id, in: hero)
                .zIndex(1)
        } else {
            coverArt
        }
        #else
        coverArt
        #endif
    }

    private var coverArt: some View {
        CachedCover(url: shown.thumbnailURL.flatMap { URL(string: $0) })
            .frame(width: coverWidth, height: coverHeight)
            .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 14, style: .continuous)
                    .strokeBorder(.white.opacity(0.12))
            }
            .shadow(color: .black.opacity(coverArtTheme ? 0.35 : 0), radius: 18, y: 8)
    }

    private var coverWidth: CGFloat {
        #if os(macOS)
        160
        #else
        coverArtTheme ? 150 : 100
        #endif
    }

    private var coverHeight: CGFloat { coverWidth * 1.45 }

    private var altTitlesLine: String? {
        let titles = shown.altTitles
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty && $0 != shown.title }
        guard !titles.isEmpty else { return nil }
        return titles.prefix(3).joined(separator: " · ")
    }

    private var metadata: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                if !shown.status.isEmpty && shown.status != "Unknown" {
                    statusBadge
                }

                if let detailLine {
                    Text(detailLine)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }

            if let byline {
                Text(byline)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
    }

    /// Plain keeps a solid, compact pill so it reads as a plain-theme label;
    /// Cover Art keeps the softer translucent badge over the backdrop.
    private var statusBadge: some View {
        Text(shown.status)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(plainBadge ? Color.white : statusTint)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(plainBadge ? AnyShapeStyle(statusTint) : AnyShapeStyle(statusTint.opacity(0.14)), in: Capsule())
            .overlay {
                if !plainBadge {
                    Capsule().strokeBorder(statusTint.opacity(0.25))
                }
            }
    }

    private var plainBadge: Bool {
        #if os(iOS)
        !coverArtTheme
        #else
        false
        #endif
    }

    private var statusTint: Color {
        switch shown.status.lowercased() {
        case "ongoing", "releasing":
            return .green
        case "completed":
            return .blue
        case "hiatus", "on hold":
            return .orange
        case "cancelled", "canceled", "dropped":
            return .red
        default:
            return .secondary
        }
    }

    private var detailLine: String? {
        var parts: [String] = []
        if let years { parts.append(years) }
        if let demographic = shown.demographic, !demographic.isEmpty { parts.append(demographic) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private var byline: String? {
        var parts: [String] = []
        if let author = shown.author, !author.isEmpty {
            parts.append("Written by \(author)")
        }
        if let artist = shown.artist, !artist.isEmpty, artist != shown.author {
            parts.append("Art by \(artist)")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private var years: String? {
        guard let start = shown.year else { return nil }
        if let end = shown.endYear, end > start { return "\(start)–\(end)" }
        return "\(start)"
    }

    @ViewBuilder
    private var readingAction: some View {
        let reading = readingChapters
        if let chapter = progress?.chapter ?? reading.last {
            Button {
                if let progress {
                    continueReading(progress)
                } else {
                    onOpen(chapter, reading)
                }
            } label: {
                VStack(spacing: 2) {
                    Label(progress == nil ? "Read First Chapter" : "Continue Reading", systemImage: "book")
                        .fontWeight(.semibold)
                    if let progress {
                        Text("\(chapterTitle(chapter)) · Page \(progress.pageIndex + 1)")
                            .font(.caption)
                    }
                }
                .frame(maxWidth: .infinity, minHeight: 32)
                .padding(.vertical, 3)
            }
            .buttonStyle(.borderedProminent)
        }
    }

    @ViewBuilder
    private var tags: some View {
        let names = shown.genreTags.isEmpty ? manga.genreTags : shown.genreTags
        if !names.isEmpty {
            VStack(alignment: .leading, spacing: 7) {
                Text("Tags")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .accessibilityAddTraits(.isHeader)
                ScrollView(.horizontal) {
                    HStack(spacing: 6) {
                        ForEach(names, id: \.self) { name in
                            Text(name)
                                .font(.caption)
                                .padding(.horizontal, 10)
                                .padding(.vertical, 5)
                                .background(.quaternary, in: Capsule())
                        }
                    }
                }
                .scrollIndicators(.hidden)
            }
        }
    }

    private func about(_ text: String) -> some View {
        Button {
            descriptionExpanded.toggle()
        } label: {
            VStack(alignment: .leading, spacing: 5) {
                HStack {
                    Text("About")
                        .font(.caption.weight(.semibold))
                    Spacer()
                    Label(descriptionExpanded ? "Show Less" : "Read More",
                          systemImage: descriptionExpanded ? "chevron.up" : "chevron.down")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.tint)
                }

                Text(text)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(descriptionExpanded ? nil : 2)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .multilineTextAlignment(.leading)
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.primary.opacity(0.045), in: RoundedRectangle(cornerRadius: 10))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private var chapterList: some View {
        let visible = visibleChapters
        let lastReadID = progress?.chapter(in: visible)?.id
        return ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    summary
                    chapterSection(visible, lastReadID: lastReadID) {
                        if let lastReadID { proxy.scrollTo(lastReadID, anchor: .center) }
                    }
                }
                .padding(.bottom, 32)
            }
            .scrollIndicators(.never)
            #if os(iOS)
            .refreshable { await reload() }
            #endif
        }
    }

    private func chapterSection(_ visible: [Chapter], lastReadID: String?, jumpToLastRead: @escaping () -> Void) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 10) {
                Text(chapterCount(visible))
                    .font(.headline)

                Spacer(minLength: 8)

                if !chapters.isEmpty {
                    chapterMenu(jumpToLastRead: lastReadID == nil ? nil : jumpToLastRead)
                }
            }
            .padding(.horizontal, 20)
            .padding(.bottom, 6)

            if loading && chapters.isEmpty {
                ProgressView()
                    .frame(maxWidth: .infinity)
                    .padding(40)
            } else if let message, chapters.isEmpty {
                Text(message)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: .infinity)
                    .padding(40)
            } else if visible.isEmpty {
                Text(chapters.isEmpty ? "No chapters yet." : "No chapters match these filters.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity)
                    .padding(40)
            } else {
                chapterRows(visible, lastReadID: lastReadID)
            }
        }
        .padding(.top, 4)
    }

    private func chapterCount(_ visible: [Chapter]) -> String {
        if loading && chapters.isEmpty { return "Loading chapters…" }
        return visible.count == 1 ? "1 chapter" : "\(visible.count) chapters"
    }

    /// Build the visible list once per render instead of sorting it again
    /// inside every row's tap action.
    private func chapterRows(_ visible: [Chapter], lastReadID: String?) -> some View {
        // Unread is a list filter, not a change to the chapter sequence.
        let candidates = chapterListCandidates
        let reading = Self.newestFirst(hideDuplicates ? Self.preferredReleases(candidates) : candidates)
        let extras = alternates
        return LazyVStack(spacing: 0) {
            ForEach(visible) { chapter in
                row(chapter, reading: reading,
                    alternates: chapter.number.flatMap { extras[$0] } ?? [],
                    isLastRead: chapter.id == lastReadID)
                    .id(chapter.id)
                Divider()
                    .padding(.leading, 20)
            }
        }
    }

    private func chapterMenu(jumpToLastRead: (() -> Void)?) -> some View {
        Menu {
            Section("Chapter Actions") {
                let reading = readingChapters
                if let latest = reading.first, reading.count > 1, latest.id != progress?.chapter.id {
                    Button {
                        onOpen(latest, reading)
                    } label: {
                        Label("Read Latest Chapter", systemImage: "book")
                    }
                }
                if let chapter = nextUnreadDownload {
                    Button {
                        startDownload(chapter)
                    } label: {
                        Label("Download Next Unread", systemImage: "arrow.down.circle")
                    }
                    .disabled(run != nil)
                }
                if let jumpToLastRead { Button("Jump to Last Read", action: jumpToLastRead) }
            }
            Section("Sort") {
                Toggle("Newest first", isOn: Binding(
                    get: { !sortAscending },
                    set: { selected in
                        if selected { sortAscending = false }
                    }
                ))

                Toggle("Oldest first", isOn: Binding(
                    get: { sortAscending },
                    set: { selected in
                        if selected { sortAscending = true }
                    }
                ))
            }

            Section("Filter") {
                Toggle("One release per chapter", isOn: $hideDuplicates)
                Toggle("Unread only", isOn: $unreadOnly)
                Toggle("Downloaded only", isOn: $downloadedOnly)

                if !scanlators.isEmpty {
                    Menu("Scanlator") {
                        ForEach(scanlators, id: \.self) { name in
                            Toggle(name, isOn: Binding(
                                get: { scanlatorFilter.contains(name) },
                                set: { setScanlator(name, selected: $0) }
                            ))
                        }
                    }
                }

                if !scanlatorFilter.isEmpty {
                    Divider()
                    Button("Clear scanlator filter") {
                        scanlatorFilter.removeAll()
                    }
                }
            }
        } label: {
            Image(systemName: "line.3.horizontal.decrease")
                .font(.system(size: 18))
                .frame(width: 32, height: 32)
                .contentShape(Rectangle())
        }
        #if os(macOS)
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        #endif
        .foregroundStyle(.tint)
        .help("Chapter actions, sorting, and filters")
        .accessibilityLabel("Chapter options")
    }

    private func setScanlator(_ name: String, selected: Bool) {
        if selected {
            scanlatorFilter.insert(name)
        } else {
            scanlatorFilter.remove(name)
        }
    }

    private var pageBackground: Color {
        #if os(macOS)
        Color(nsColor: .windowBackgroundColor)
        #else
        Color(uiColor: .systemBackground)
        #endif
    }

    private func row(_ chapter: Chapter, reading: [Chapter], alternates: [Chapter], isLastRead: Bool) -> some View {
        HStack(spacing: 8) {
            Button {
                onOpen(chapter, reading)
            } label: {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 8) {
                        Text(chapterTitle(chapter))
                            .font(.system(size: 16))
                            .lineLimit(1)
                        if isLastRead {
                            Text("Last read").font(.caption.weight(.semibold)).foregroundStyle(.tint)
                        }
                    }

                    if let subtitle = chapterSubtitle(chapter) {
                        Text(subtitle)
                            .font(.system(size: 14))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if !alternates.isEmpty {
                Menu {
                    ForEach(alternates) { other in
                        Button(alternateLabel(other)) {
                            onOpen(other, Self.replacing(chapter, with: other, in: reading))
                        }
                    }
                } label: {
                    Text("\(alternates.count) more")
                        .font(.caption2.weight(.medium))
                        .foregroundStyle(.secondary)
                        .padding(.horizontal, 7)
                        .padding(.vertical, 3)
                        .background(Color.primary.opacity(0.06), in: Capsule())
                }
                .menuIndicator(.hidden)
                .tint(.secondary)
                .fixedSize()
                .help("Other releases of this chapter")
                .accessibilityLabel("\(alternates.count) other releases")
            }

            downloadControl(chapter)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 8)
        .background(isLastRead ? Color.accentColor.opacity(0.10) : .clear)
    }

    private func alternateLabel(_ chapter: Chapter) -> String {
        scanlatorName(chapter) ?? chapterTitle(chapter)
    }

    /// Opens an alternate release in the slot its chapter number occupies, so
    /// the reader keeps the surrounding reading order.
    private static func replacing(_ primary: Chapter, with alternate: Chapter, in reading: [Chapter]) -> [Chapter] {
        guard let index = reading.firstIndex(where: { $0.id == primary.id }) else { return reading }
        var list = reading
        list[index] = alternate
        return list
    }

    private func chapterTitle(_ chapter: Chapter) -> String {
        let name = chapter.name.trimmingCharacters(in: .whitespacesAndNewlines)
        if name.isEmpty {
            if let number = chapter.number {
                return "Chapter \(formatNumber(number))"
            }
            return "Chapter"
        }
        if name.hasSuffix(".0") {
            return String(name.dropLast(2))
        }
        return name
    }

    private func chapterSubtitle(_ chapter: Chapter) -> String? {
        var parts: [String] = []
        let name = chapter.name.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        let titleHasNumber = name.hasPrefix("chapter")
            || name.hasPrefix("ch.")
            || (chapter.number.map { name.contains(formatNumber($0)) } ?? false)
        if let number = chapter.number, !titleHasNumber {
            parts.append("Chapter \(formatNumber(number))")
        }
        if let scanlator = scanlatorName(chapter) {
            parts.append(scanlator)
        }
        if chapter.uploadedAt > 0 {
            parts.append(uploadAge(chapter.uploadedAt))
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    /// "3d ago" from the source's upload timestamp, which travels with the
    /// cached chapter list.
    private func uploadAge(_ timestamp: Int) -> String {
        Self.uploadFormatter.localizedString(
            for: Date(timeIntervalSince1970: TimeInterval(timestamp)),
            relativeTo: Date()
        )
    }

    private static let uploadFormatter: RelativeDateTimeFormatter = {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .short
        return formatter
    }()

    private func formatNumber(_ number: Float) -> String {
        number == number.rounded() ? String(format: "%.0f", number) : String(format: "%.1f", number)
    }

    @ViewBuilder
    private func downloadControl(_ chapter: Chapter) -> some View {
        if savedChapters.contains(chapter.url) {
            Image(systemName: "arrow.down.circle.fill")
                .foregroundStyle(.tertiary)
                .imageScale(.small)
                .frame(width: 24)
                .help("Saved for offline reading")
        } else if run?.chapterURL == chapter.url {
            HStack(spacing: 8) {
                ProgressView()
                    .controlSize(.small)
                if let job = run?.job {
                    Button {
                        stopDownload(job)
                    } label: {
                        Image(systemName: "xmark")
                            .contentShape(Circle())
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(.tertiary)
                    .help("Stop downloading this chapter")
                }
            }
        } else {
            Button {
                startDownload(chapter)
            } label: {
                Image(systemName: "arrow.down.circle")
                    .foregroundStyle(.tertiary)
                    .imageScale(.small)
                    .contentShape(Circle())
                    .frame(width: 24, height: 24)
            }
            .buttonStyle(.plain)
            .disabled(run != nil)
            .help("Download this chapter")
        }
    }

    private var cacheKey: String { "\(sourceID):\(manga.url)" }

    private func load() async {
        if let cached = await ChapterListCache.shared.chapters(for: cacheKey) {
            chapters = cached
            loading = false
        }
        await reload()
    }

    #if os(macOS)
    private func refresh() {
        guard !reloading, !refreshing else { return }
        refreshing = true
        Task {
            await reload()
            refreshing = false
        }
    }
    #endif

    /// Chapters, details and the saved-chapter list do not depend on each
    /// other, so load them together. The chapter list stops showing its
    /// spinner as soon as that request returns.
    private func reload() async {
        guard !reloading else { return }
        reloading = true
        defer { reloading = false }

        if chapters.isEmpty {
            loading = true
        }
        message = nil
        downloadFailure = nil

        async let chaptersLoaded: Void = loadChapters()
        async let detailsLoaded: Void = loadDetails()
        async let savedLoaded: Void = loadSaved()
        _ = await (chaptersLoaded, detailsLoaded, savedLoaded)
    }

    private func loadChapters() async {
        let key = cacheKey
        let sourceID = self.sourceID
        let manga = self.manga
        do {
            let loaded = try await Task.detached(priority: .userInitiated) {
                try Core.chapters(sourceID: sourceID, manga: manga)
            }.value
            chapters = loaded
            scanlatorFilter.formIntersection(Set(loaded.compactMap { scanlatorName($0) }))
            loading = false
            message = nil
            await ChapterListCache.shared.store(loaded, for: key)
        } catch {
            loading = false
            if chapters.isEmpty {
                message = error.localizedDescription
            } else {
                downloadFailure = "Refresh failed: \(error.localizedDescription)"
            }
        }
    }

    private func loadDetails() async {
        let sourceID = self.sourceID
        let manga = self.manga
        let loaded = try? await Task.detached(priority: .utility) {
            try Core.details(sourceID: sourceID, manga: manga)
        }.value
        if let loaded {
            detail = loaded
        }
    }

    private func loadSaved() async {
        let sourceID = self.sourceID
        let library = try? await Task.detached(priority: .utility) {
            try Core.downloads(sourceID: sourceID)
        }.value
        if let library {
            savedChapters = savedURLs(library)
            loadedSavedChapters = true
        }
    }

    private func savedURLs(_ library: [DownloadedManga]) -> Set<String> {
        Set(library.flatMap { $0.chapters.map(\.url) })
    }

    private func startDownload(_ chapter: Chapter) {
        guard run == nil, !savedChapters.contains(chapter.url) else { return }
        downloadFailure = nil
        downloadNote = "Starting download…"
        run = DownloadRun(chapterURL: chapter.url)
        let manga = shown
        let sourceID = self.sourceID
        Task {
            do {
                let started = try await Task.detached(priority: .utility) {
                    try Core.startDownload(sourceID: sourceID, manga: manga, chapters: [chapter])
                }.value
                run?.job = started
                await watch(started)
            } catch {
                downloadFailure = error.localizedDescription
                downloadNote = nil
                run = nil
            }
        }
    }

    private func watch(_ handle: UInt64) async {
        while run?.job == handle {
            try? await Task.sleep(for: .milliseconds(400))
            let polled = try? await Task.detached(priority: .utility) {
                try Core.downloadStatus(job: handle)
            }.value
            guard let status = polled else {
                Core.releaseDownload(job: handle)
                run = nil
                break
            }
            downloadNote = status.error ?? status.message
            guard !status.running else { continue }
            if run?.cancelled == true {
                downloadNote = "Download cancelled"
            } else if let failure = status.error {
                downloadFailure = failure
                downloadNote = nil
            }
            Core.releaseDownload(job: handle)
            await loadSaved()
            run = nil
            break
        }
    }

    private func stopDownload(_ job: UInt64) {
        run?.cancelled = true
        downloadNote = "Stopping download…"
        Core.cancelDownload(job: job)
    }

}
