import SwiftUI
import ImageIO

@MainActor
final class CoverCache {
    static let shared = CoverCache()

    /// Decoded pixels, so a card scrolling back into view does not decode its
    /// JPEG again. `NSCache` evicts by the total pixel cost on its own.
    private let decoded = NSCache<NSURL, CGImage>()
    private let cache = URLCache(memoryCapacity: 16 * 1024 * 1024, diskCapacity: 100 * 1024 * 1024, diskPath: "rustga-covers")
    private lazy var session: URLSession = {
        let config = URLSessionConfiguration.default
        config.urlCache = cache
        config.requestCachePolicy = .returnCacheDataElseLoad
        return URLSession(configuration: config)
    }()

    init() {
        decoded.totalCostLimit = 128 * 1024 * 1024
    }

    func cachedImage(for url: URL) -> CGImage? {
        decoded.object(forKey: url as NSURL)
    }

    func image(for url: URL) async throws -> CGImage? {
        if let cached = cachedImage(for: url) { return cached }

        // The card's `.task` cancels when it leaves the screen, which cancels
        // the request. Check again before decoding so bytes that arrived for a
        // card that has already gone do not start a decode.
        try Task.checkCancellation()
        let (data, response) = try await session.data(from: url)
        guard let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode) else {
            throw URLError(.badServerResponse)
        }
        try Task.checkCancellation()

        let image = await Task.detached(priority: .utility) { () -> CGImage? in
            guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
            return CGImageSourceCreateThumbnailAtIndex(source, 0, [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceThumbnailMaxPixelSize: 600,
                kCGImageSourceShouldCacheImmediately: true
            ] as CFDictionary)
        }.value
        guard let image else { return nil }
        decoded.setObject(image, forKey: url as NSURL, cost: image.width * image.height * 4)
        return image
    }

    func clear() {
        cache.removeAllCachedResponses()
        decoded.removeAllObjects()
    }
}

struct CachedCover: View {
    let url: URL?
    @State private var image: CGImage?

    var body: some View {
        Group {
            if let image {
                Image(decorative: image, scale: 1).resizable().scaledToFill()
            } else {
                Rectangle().fill(.quaternary)
            }
        }
        .task(id: url) {
            guard let url else {
                image = nil
                return
            }
            if let cached = CoverCache.shared.cachedImage(for: url) {
                image = cached
                return
            }
            image = nil
            do {
                let loaded = try await CoverCache.shared.image(for: url)
                if !Task.isCancelled { image = loaded }
            } catch {
                image = nil
            }
        }
    }
}

private enum HomeTab: String, CaseIterable, Identifiable {
    case browse = "Browse"
    case library = "Library"
    case downloads = "Downloads"
    case discover = "Discover"
    #if os(iOS)
    case settings = "Settings"
    #endif

    var id: Self { self }

    var icon: String {
        switch self {
        case .browse: return "square.grid.2x2"
        case .library: return "books.vertical"
        case .downloads: return "arrow.down.circle"
        case .discover: return "binoculars"
        #if os(iOS)
        case .settings: return "gearshape"
        #endif
        }
    }
}

/// The tab switch with a Liquid Glass selection pill: it slides between tabs,
/// follows a press-drag, and lifts the tab under the pointer.
private struct TabBar: View {
    @Binding var selection: HomeTab
    let discoverIcon: String

    @Namespace private var pill
    @State private var hovered: HomeTab?
    @State private var frames: [HomeTab: CGRect] = [:]
    @GestureState private var dragging = false

    var body: some View {
        GlassEffectContainer(spacing: 12) {
            HStack(spacing: 2) {
                ForEach(HomeTab.allCases) { item in
                    button(item)
                }
            }
            .padding(4)
            .glassEffect(.regular.interactive(), in: Capsule())
            .simultaneousGesture(drag)
            .animation(.smooth(duration: 0.24), value: selection)
            .animation(.easeOut(duration: 0.12), value: hovered)
            .animation(.easeOut(duration: 0.12), value: dragging)
        }
        .shadow(color: .black.opacity(0.3), radius: 14, y: 8)
    }

    private func button(_ item: HomeTab) -> some View {
        Button {
            selection = item
        } label: {
            Label(item.rawValue, systemImage: item == .discover ? discoverIcon : item.icon)
                .font(.callout)
                .foregroundStyle(selection == item ? Color.primary : Color.secondary)
                .scaleEffect(hovered == item ? 1.06 : 1)
                .padding(.horizontal, 11)
                .padding(.vertical, 5)
                .background {
                    if selection == item {
                        Capsule()
                            .fill(Color.primary.opacity(dragging ? 0.16 : 0.11))
                            .matchedGeometryEffect(id: "selection", in: pill)
                    } else if hovered == item {
                        Capsule().fill(Color.primary.opacity(0.06))
                    }
                }
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frames[item] = $0 }
        .onContinuousHover { phase in
            switch phase {
            case .active:
                hovered = item
            case .ended:
                if hovered == item {
                    hovered = nil
                }
            }
        }
    }

    private var drag: some Gesture {
        DragGesture(minimumDistance: 0, coordinateSpace: .global)
            .updating($dragging) { _, active, _ in active = true }
            .onChanged { value in
                let item = tab(at: value.location)
                hovered = item
                if let item { selection = item }
            }
    }

    private func tab(at point: CGPoint) -> HomeTab? {
        frames.first { $0.value.contains(point) }?.key
    }
}

struct ContentView: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.openURL) private var openURL
    @State private var sources: [SourceRow] = []
    @State private var selection: UInt64 = 0
    @State private var tab: HomeTab = .browse
    @State private var query = ""
    @State private var results: [Manga] = []
    @State private var sections: [MangaSection] = []
    @State private var activeQuery: String?
    @State private var failure: String?
    @State private var busy = false
    @State private var showSettings = false
    @State private var showClearHistory = false
    @Namespace private var seriesZoom
    #if os(iOS)
    @State private var searchVisible = true
    @Namespace private var readerZoom
    #endif

    @State private var series: Manga?
    @State private var seriesSourceID: UInt64 = 0
    @State private var seriesReader: ReadSession?
    @State private var homeReader: ReadSession?
    @State private var resuming = false
    @AppStorage("discoverGlobeIcon") private var discoverGlobeIcon = false
    @AppStorage("showBrowseTab") private var showBrowseTab = true
    @AppStorage("showLibraryTab") private var showLibraryTab = true
    @AppStorage("showDownloadsTab") private var showDownloadsTab = true
    @AppStorage("showDiscoverTab") private var showDiscoverTab = true
    @AppStorage("hideDuplicateReleases") private var hideDuplicates = true
    @AppStorage("readingHistory") private var readingHistory = Data()
    @AppStorage("savedManga") private var savedManga = Data()
    @AppStorage("seriesCoverArtTheme") private var seriesCoverArtTheme = false

    private struct ReadSession: Identifiable {
        let sourceID: UInt64
        let manga: Manga
        let chapter: Chapter
        let chapters: [Chapter]
        var resume: ReadingProgress? = nil
        var downloaded = false

        var id: String { chapter.id }
    }

    // `globe` is the wireframe circle with meridians, not an earth map.
    private var discoverIcon: String { discoverGlobeIcon ? "globe" : HomeTab.discover.icon }

    #if os(iOS)
    /// Outline at rest; the filled variant only while that tab is active.
    private func tabLabel(_ title: String, icon: String, item: HomeTab) -> some View {
        Label {
            Text(title)
        } icon: {
            Image(systemName: icon)
                .environment(\.symbolVariants, tab == item ? .fill : .none)
        }
    }

    private var visibleTabs: [HomeTab] {
        var tabs: [HomeTab] = []
        if showBrowseTab { tabs.append(.browse) }
        if showLibraryTab { tabs.append(.library) }
        if showDownloadsTab { tabs.append(.downloads) }
        if showDiscoverTab { tabs.append(.discover) }
        tabs.append(.settings)
        return tabs
    }
    #endif
    private var history: [ReadingProgress] {
        (try? JSONDecoder().decode([ReadingProgress].self, from: readingHistory)) ?? []
    }

    /// Bumped on every load so a slow reply for an old source or query is
    /// dropped instead of overwriting the current results.
    @State private var request = 0

    private let columns = [GridItem(.adaptive(minimum: 160), spacing: 6)]

    var body: some View {
        Group {
            #if os(iOS)
            home
                .fullScreenCover(item: $homeReader) { session in
                    reader(session) { homeReader = nil }
                }
                .fullScreenCover(item: $series) { selected in
                    seriesPage(selected)
                        .navigationTransition(.zoom(sourceID: tab == .library
                            ? SavedManga(sourceID: seriesSourceID, manga: selected).id : selected.id, in: seriesZoom))
                        .fullScreenCover(item: $seriesReader) { session in
                            reader(session) { seriesReader = nil }
                                .navigationTransition(.zoom(sourceID: session.chapter.id, in: readerZoom))
                        }
                }
            #else
            // Keep the grid mounted underneath the series page. That preserves
            // its scroll position and gives the cover a live source view for the
            // matched-geometry transition in both directions.
            ZStack {
                home

                if let series {
                    seriesPage(series)
                    .transition(.opacity)
                    .zIndex(1)
                }

                if let session = seriesReader ?? homeReader {
                    reader(session) {
                        seriesReader = nil
                        homeReader = nil
                    }
                    .transition(.opacity)
                    .zIndex(2)
                }
            }
            #endif
        }
        #if os(macOS)
        .frame(minWidth: 760, minHeight: 560)
        #endif
        .animation(reduceMotion ? nil : .spring(response: 0.42, dampingFraction: 0.86), value: series?.id)
        .animation(reduceMotion ? nil : .easeInOut(duration: 0.2), value: (seriesReader ?? homeReader)?.id)
        .sheet(isPresented: $showSettings) {
            VStack(spacing: 20) {
                settings
                Button("Done") { showSettings = false }
            }
            .padding(24)
        }
        .task { loadSources() }
        .onChange(of: selection) { _, _ in load() }
    }

    private var settings: some View {
        VStack(spacing: 20) {
            Text("Settings").font(.title2.bold())
            Toggle("Use the internet globe for Discover", isOn: $discoverGlobeIcon)
            #if os(iOS)
            VStack(alignment: .leading, spacing: 8) {
                Text("Tab bar")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .textCase(.uppercase)
                Toggle("Browse", isOn: $showBrowseTab)
                Toggle("Library", isOn: $showLibraryTab)
                Toggle("Downloads", isOn: $showDownloadsTab)
                Toggle("Discover", isOn: $showDiscoverTab)
                Text("Settings always stays in the tab bar.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            #endif
            Picker("Series appearance", selection: $seriesCoverArtTheme) {
                Text("Plain").tag(false)
                Text("Cover Art").tag(true)
            }
            .pickerStyle(.menu)
            Button("Clear reading history", role: .destructive) { showClearHistory = true }
            Button("Clear cover and page cache") {
                CoverCache.shared.clear()
                PageImages.shared.clear()
                Task { await PageCache.shared.clear() }
            }
            Text("Downloaded chapters are not affected.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .padding(24)
        .confirmationDialog("Clear reading history?", isPresented: $showClearHistory, titleVisibility: .visible) {
            Button("Clear History", role: .destructive) { readingHistory = Data() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Continue Reading and last-read markers will be removed.")
        }
    }

    private var home: some View {
        #if os(iOS)
        TabView(selection: $tab) {
            if showBrowseTab {
                Tab(value: .browse) {
                    browse
                } label: {
                    tabLabel("Browse", icon: HomeTab.browse.icon, item: .browse)
                }
            }
            if showLibraryTab {
                Tab(value: .library) {
                    library
                } label: {
                    tabLabel("Library", icon: HomeTab.library.icon, item: .library)
                }
            }
            if showDownloadsTab {
                Tab(value: .downloads) {
                    downloads
                } label: {
                    tabLabel("Downloads", icon: HomeTab.downloads.icon, item: .downloads)
                }
            }
            if showDiscoverTab {
                Tab(value: .discover) {
                    discoverView
                } label: {
                    tabLabel("Discover", icon: discoverIcon, item: .discover)
                }
            }
            Tab(value: .settings) {
                settings
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            } label: {
                tabLabel("Settings", icon: HomeTab.settings.icon, item: .settings)
            }
        }
        .onChange(of: visibleTabs, initial: true) { _, tabs in
            if !tabs.contains(tab) { tab = tabs.first ?? .browse }
        }
        #else
        ZStack(alignment: .top) {
            if tab == .browse {
                browse
            } else if tab == .discover {
                discoverView
            } else if tab == .library {
                library
            } else {
                downloads
            }

            // Top alignment keeps the tab switch at a fixed height while the
            // search bar comes and goes.
            HStack(alignment: .top, spacing: 12) {
                TabBar(selection: $tab, discoverIcon: discoverIcon)
                Spacer(minLength: 12)
                if tab == .browse {
                    searchBar
                }
            }
            .padding(.horizontal, 20)
            .padding(.top, 14)
        }
        #endif
    }

    private var discoverView: some View {
        DiscoverView(sourceID: selection, sources: sources) { source, manga in
            selection = source
            seriesSourceID = source
            series = manga
        }
        #if os(macOS)
        .padding(.top, 70)
        #endif
    }

    private var browse: some View {
        ScrollView {
            VStack(spacing: 16) {
                if activeQuery == nil, let recent = history.first,
                   sources.contains(where: { $0.id == recent.sourceID }) {
                    continueReading(recent)
                }
                if busy && results.isEmpty && sections.isEmpty {
                    skeleton
                } else if activeQuery == nil && browseOnly {
                    if sections.isEmpty {
                        emptyState
                    } else {
                        shelves
                    }
                } else if results.isEmpty {
                    emptyState
                } else {
                    grid
                }
            }
        }
        .scrollIndicators(.never)
        #if os(iOS)
        .safeAreaInset(edge: .top, spacing: 0) {
            Color.clear
                .frame(height: 1)
                .background(Color(uiColor: .systemBackground), ignoresSafeAreaEdges: .top)
                .accessibilityHidden(true)
        }
        .contentMargins(.top, 60, for: .scrollContent)
        .contentMargins(.bottom, 24, for: .scrollContent)
        .onScrollGeometryChange(for: Int.self) { geometry in
            let offset = geometry.contentOffset.y + geometry.contentInsets.top
            let maximum = max(0, geometry.contentSize.height + geometry.contentInsets.top + geometry.contentInsets.bottom - geometry.containerSize.height)
            return Int((min(max(offset, 0), maximum) / 4).rounded())
        } action: { oldOffset, newOffset in
            guard oldOffset != newOffset else { return }
            let visible = newOffset == 0 || newOffset < oldOffset
            if visible != searchVisible {
                withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.2)) {
                    searchVisible = visible
                }
            }
        }
        .overlay(alignment: .top) {
            searchBar
                .padding(.horizontal, 12)
                .offset(y: searchVisible ? 0 : -80)
                .opacity(searchVisible ? 1 : 0)
                .allowsHitTesting(searchVisible)
                .accessibilityHidden(!searchVisible)
        }
        #else
        .contentMargins(.top, 80, for: .scrollContent)
        #endif
    }

    private var grid: some View {
        LazyVGrid(columns: columns, spacing: 10) {
            ForEach(results) { manga in
                Button {
                    open(manga)
                } label: {
                    #if os(macOS)
                    CoverCard(manga: manga, hero: seriesZoom)
                        .equatable()
                    #else
                    CoverCard(manga: manga)
                        .equatable()
                        .matchedTransitionSource(id: manga.id, in: seriesZoom)
                    #endif
                }
                .buttonStyle(CardButtonStyle())
            }
        }
        .padding(.horizontal, 14)
        .padding(.bottom, 22)
    }

    /// Homepage shelves for a browse-only source such as Comick, whose feeds
    /// carry more than one ranking.
    private var shelves: some View {
        LazyVStack(alignment: .leading, spacing: 24) {
            ForEach(sections) { section in
                VStack(alignment: .leading, spacing: 10) {
                    Text(section.title)
                        .font(.title3.bold())
                        .padding(.horizontal, 14)

                    ScrollView(.horizontal) {
                        LazyHStack(alignment: .top, spacing: 2) {
                            ForEach(section.mangas) { manga in
                                Button {
                                    open(manga)
                                } label: {
                                    CoverCard(manga: manga)
                                        .equatable()
                                        .frame(width: 148)
                                }
                                .buttonStyle(CardButtonStyle())
                            }
                        }
                        .padding(.horizontal, 10)
                    }
                    .scrollIndicators(.never)
                }
            }
        }
        .padding(.top, 4)
        .padding(.bottom, 22)
    }

    private func open(_ manga: Manga) {
        if browseOnly {
            if let url = URL(string: manga.url) {
                openURL(url)
            }
            return
        }
        seriesSourceID = selection
        series = manga
    }

    private func seriesPage(_ manga: Manga) -> some View {
        let sourceID = seriesSourceID
        #if os(macOS)
        let hero: Namespace.ID? = results.contains(where: { $0.id == manga.id }) && tab == .browse ? seriesZoom : nil
        #else
        let hero: Namespace.ID? = nil
        #endif
        let progress = history.first { $0.matches(sourceID: sourceID, manga: manga) }
        return SeriesView(manga: manga, sourceID: sourceID, hero: hero, progress: progress) { chapter, chapters in
            seriesReader = ReadSession(sourceID: sourceID, manga: manga, chapter: chapter, chapters: chapters,
                                       resume: progress?.chapter.id == chapter.id ? progress : nil)
        } onBack: {
            seriesReader = nil
            series = nil
        }
    }

    private var library: some View {
        let entries = SavedManga.list(in: savedManga)
        return Group {
            if entries.isEmpty {
                ContentUnavailableView("No Saved Manga", systemImage: HomeTab.library.icon,
                                       description: Text("Use the bookmark on a series page to save it here."))
            } else {
                ScrollView {
                    LazyVGrid(columns: columns, spacing: 10) {
                        ForEach(entries) { entry in
                            Button {
                                seriesSourceID = entry.sourceID
                                series = entry.manga
                            } label: {
                                VStack(alignment: .leading, spacing: 0) {
                                    CoverCard(manga: entry.manga)
                                        .equatable()
                                        #if os(iOS)
                                        .matchedTransitionSource(id: entry.id, in: seriesZoom)
                                        #endif
                                    if let source = sources.first(where: { $0.id == entry.sourceID }) {
                                        Text(source.name)
                                            .font(.caption)
                                            .foregroundStyle(.secondary)
                                            .lineLimit(1)
                                            .padding(.horizontal, 8)
                                    }
                                }
                            }
                            .buttonStyle(CardButtonStyle())
                        }
                    }
                    .padding(.horizontal, 14)
                    .padding(.vertical, 12)
                }
                .scrollIndicators(.never)
            }
        }
        #if os(macOS)
        .padding(.top, 70)
        #endif
    }

    private var downloads: some View {
        let sourceID = selection
        return DownloadsView(sourceID: sourceID, sourceName: sourceName) { manga, chapter, chapters in
            let progress = history.first { $0.matches(sourceID: sourceID, manga: manga) && $0.chapter.id == chapter.id }
            homeReader = ReadSession(sourceID: sourceID, manga: manga, chapter: chapter, chapters: chapters,
                                     resume: progress, downloaded: true)
        }
    }

    private func reader(_ session: ReadSession, onBack: @escaping () -> Void) -> some View {
        ReaderView(chapter: session.chapter, sourceID: session.sourceID, chapters: session.chapters,
                   resume: session.resume) { chapter, pageIndex, finished in
            var updated = history
            let previous = updated.first { $0.matches(sourceID: session.sourceID, manga: session.manga) }
            var completed = previous?.completedChapters
            if finished {
                var keys = completed ?? []
                keys.insert(chapter.readingKey)
                completed = keys
            }
            let progress = ReadingProgress(sourceID: session.sourceID, manga: session.manga,
                                           chapter: chapter, pageIndex: pageIndex, downloaded: session.downloaded,
                                           completedChapters: completed)
            guard updated.first != progress else { return }
            updated.removeAll { $0.matches(sourceID: session.sourceID, manga: session.manga) }
            updated.insert(progress, at: 0)
            if let encoded = try? JSONEncoder().encode(updated) { readingHistory = encoded }
        } onBack: { onBack() }
    }

    private func continueReading(_ recent: ReadingProgress) -> some View {
        Button {
            resuming = true
            Task {
                let chapters = await resumeChapters(for: recent)
                defer { resuming = false }
                guard tab == .browse, series == nil, homeReader == nil else { return }
                homeReader = ReadSession(sourceID: recent.sourceID, manga: recent.manga, chapter: recent.chapter,
                                         chapters: chapters, resume: recent, downloaded: recent.downloaded)
            }
        } label: {
            HStack(spacing: 12) {
                Image(systemName: "book")
                VStack(alignment: .leading, spacing: 4) {
                    Text("Continue Reading").font(.headline)
                    Text(recent.manga.title).lineLimit(1)
                    Text("\(recent.chapter.name.isEmpty ? "Chapter" : recent.chapter.name) · Page \(recent.pageIndex + 1)")
                        .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer()
                if resuming { ProgressView().controlSize(.small) }
                else { Image(systemName: "chevron.right").foregroundStyle(.secondary) }
            }
            .padding(14)
            .background(.quaternary, in: RoundedRectangle(cornerRadius: 14))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(resuming)
        .padding(.horizontal, 14)
    }

    private func resumeChapters(for recent: ReadingProgress) async -> [Chapter] {
        let chapters: [Chapter]
        if recent.downloaded {
            chapters = await Task.detached(priority: .userInitiated) {
                let library = try? Core.downloads(sourceID: recent.sourceID)
                return library?.first { $0.id == recent.manga.id }?.chapters ?? []
            }.value
        } else {
            let key = "\(recent.sourceID):\(recent.manga.id)"
            let loaded: [Chapter]
            if let cached = await ChapterListCache.shared.chapters(for: key), !cached.isEmpty {
                loaded = cached
            } else {
                loaded = await Task.detached(priority: .userInitiated) {
                    (try? Core.chapters(sourceID: recent.sourceID, manga: recent.manga)) ?? []
                }.value
            }
            if hideDuplicates {
                let others = loaded.filter { chapter in
                    chapter.id != recent.chapter.id && (recent.chapter.number == nil || chapter.number != recent.chapter.number)
                }
                chapters = SeriesView.preferredReleases(others)
            } else {
                chapters = loaded
            }
        }
        return SeriesView.newestFirst(chapters.filter { $0.id != recent.chapter.id } + [recent.chapter])
    }

    private var skeleton: some View {
        LazyVGrid(columns: columns, spacing: 10) {
            ForEach(0..<8, id: \.self) { _ in
                VStack(alignment: .leading, spacing: 8) {
                    Color.clear
                        .aspectRatio(2.0 / 3.0, contentMode: .fit)
                        .background(.quaternary, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    RoundedRectangle(cornerRadius: 4)
                        .fill(.quaternary)
                        .frame(height: 11)
                    RoundedRectangle(cornerRadius: 4)
                        .fill(.quaternary)
                        .frame(height: 11)
                        .padding(.trailing, 36)
                }
                .padding(8)
                .opacity(0.6)
            }
        }
        .padding(.horizontal, 14)
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Image(systemName: failure == nil ? "books.vertical" : "exclamationmark.triangle")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(.tertiary)
                .padding(.bottom, 2)

            Text(emptyTitle)
                .font(.headline)

            Text(failure ?? "Search \(sourceName) by title.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .textSelection(.enabled)

            if failure != nil {
                Button("Try Again", action: retry)
                    .buttonStyle(.glass)
                    .padding(.top, 6)
            }
        }
        .frame(maxWidth: 420)
        .frame(maxWidth: .infinity, minHeight: 420)
        .padding(.horizontal, 24)
    }

    private var emptyTitle: String {
        if failure != nil {
            return sources.isEmpty ? "No sources loaded" : "Couldn’t load \(sourceName)"
        }
        if let activeQuery {
            return "No results for “\(activeQuery)”."
        }
        return sources.isEmpty ? "Find something to read" : "Nothing popular on \(sourceName)."
    }

    private var sourceName: String {
        sources.first { $0.id == selection }?.name ?? "this source"
    }

    /// Comick and any other source that only serves catalogue feeds.
    private var browseOnly: Bool {
        sources.first { $0.id == selection }?.browseOnly ?? false
    }

    private var barContent: some View {
        HStack(spacing: 8) {
            #if os(macOS)
            Button { showSettings = true } label: {
                Image(systemName: "gearshape")
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Settings")
            #endif

            Picker("Source", selection: $selection) {
                ForEach(sources) { source in
                    Text(source.name).tag(source.id)
                }
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .frame(maxWidth: 136)
            .disabled(sources.isEmpty)

            #if os(macOS)
            Divider().frame(height: 18)
            #endif

            Image(systemName: "magnifyingglass")
                .font(.callout)
                .foregroundStyle(.secondary)

            TextField("Search titles", text: $query)
                .textFieldStyle(.plain)
                #if os(iOS)
                .submitLabel(.search)
                #endif
                .onSubmit { load(query: query) }

            if !query.isEmpty && !busy {
                Button {
                    query = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(.tertiary)
                }
                .buttonStyle(.plain)
                .help("Clear the search")
            }

            if busy {
                ProgressView()
                    .controlSize(.small)
            }

            #if os(iOS)
            Button { load(query: query) } label: {
                Image(systemName: "arrow.up.circle.fill")
                    .font(.title3)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Search")
            .disabled(busy || selection == 0 || query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            #else
            Button("Search") { load(query: query) }
                .buttonStyle(.glass)
                .disabled(busy || selection == 0)
            #endif
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
    }

    private var searchBar: some View {
        GlassEffectContainer(spacing: 16) {
            barContent
                .frame(maxWidth: 470)
                .glassEffect(.regular.interactive(), in: Capsule())
        }
        .shadow(color: .black.opacity(0.38), radius: 18, y: 10)
    }

    private func loadSources() {
        guard sources.isEmpty else { return }
        do {
            sources = try Core.sources()
            selection = sources.first?.id ?? 0
        } catch {
            failure = error.localizedDescription
        }
    }

    /// Loads popular titles, or the query when one is given. Any reply that is
    /// no longer the newest request is dropped, so a slow source cannot
    /// overwrite a result the user has already moved on from.
    private func load(query text: String? = nil) {
        guard selection != 0 else { return }
        let term = text?.trimmingCharacters(in: .whitespacesAndNewlines)
        // An empty field is not a search; keep the current grid.
        if let term, term.isEmpty { return }

        let sourceID = selection
        let showingSections = term == nil && browseOnly
        request += 1
        let token = request
        activeQuery = term
        busy = true
        failure = nil
        if term == nil {
            results = []
            sections = []
        }

        Task {
            do {
                if showingSections {
                    let loaded = try await Task.detached(priority: .userInitiated) {
                        try Core.sections(sourceID: sourceID)
                    }.value
                    guard token == request else { return }
                    sections = loaded
                } else {
                    let found = try await Task.detached(priority: .userInitiated) {
                        if let term {
                            return try Core.search(sourceID: sourceID, query: term)
                        }
                        return try Core.popular(sourceID: sourceID)
                    }.value
                    guard token == request else { return }
                    results = found
                }
            } catch {
                guard token == request else { return }
                failure = error.localizedDescription
            }
            busy = false
        }
    }

    private func retry() {
        if sources.isEmpty {
            loadSources()
        } else {
            load(query: activeQuery)
        }
    }
}

private struct HeroSource: ViewModifier {
    let id: String
    let namespace: Namespace.ID?

    func body(content: Content) -> some View {
        if let namespace {
            content.matchedGeometryEffect(id: id, in: namespace, isSource: true)
        } else {
            content
        }
    }
}

private extension View {
    func heroSource(id: String, namespace: Namespace.ID?) -> some View {
        modifier(HeroSource(id: id, namespace: namespace))
    }
}

/// Presses the whole card slightly, so choosing a title feels like touching it
/// rather than flipping a page.
private struct CardButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.97 : 1)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

struct CoverCard: View, Equatable {
    let manga: Manga
    /// The cover participates in the series-page hero transition on macOS.
    var hero: Namespace.ID? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            cover
                .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .strokeBorder(.white.opacity(0.08))
                }

            Text(manga.title)
                .font(.callout.weight(.medium))
                .lineLimit(2, reservesSpace: true)
                .multilineTextAlignment(.leading)

            Text(manga.author ?? " ")
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .padding(8)
        .contentShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    private var cover: some View {
        Color.clear
            .aspectRatio(2.0 / 3.0, contentMode: .fit)
            .overlay {
                CachedCover(url: manga.thumbnailURL.flatMap { URL(string: $0) })
            }
            .clipped()
            .heroSource(id: manga.id, namespace: hero)
    }

    private func placeholder(icon: String) -> some View {
        ZStack {
            Rectangle().fill(.quaternary)
            Image(systemName: icon)
                .font(.system(size: 20, weight: .light))
                .foregroundStyle(.tertiary)
        }
    }
}

private struct DiscoverView: View {
    let sourceID: UInt64
    let sources: [SourceRow]
    let onOpen: (UInt64, Manga) -> Void
    @State private var text = ""
    @State private var useAI = false
    @State private var endpoint = "https://api.z.ai/api/paas/v4"
    @State private var model = "glm-4-flash"
    @State private var key = ""
    @State private var recommendations: [Recommendation] = []
    @State private var busy = false
    @State private var error: String?
    @State private var searched = false
    @State private var matches: [String: Manga] = [:]
    @State private var matching: String?
    @State private var comickSections: [MangaSection] = []
    @State private var comickError: String?
    @State private var comickLoading = false
    @State private var comickLoaded = false
    /// The Comick title whose lookup is open in the results sheet.
    @State private var picked: Manga?
    /// A result waiting for the sheet to finish dismissing before it opens.
    @State private var pendingOpen: SearchMatch?
    @State private var found: [SearchMatch] = []
    @State private var finding = false
    @State private var findError: String?
    @State private var lookupSource: SourceRow?
    @State private var searchedOthers = false
    @State private var lookupID = UUID()
    /// The title in the lookup sheet's field. Comick titles often differ from
    /// the source's, so the query is editable before it is searched again.
    @State private var searchText = ""

    private struct SearchMatch: Identifiable {
        let source: SourceRow
        let manga: Manga
        var id: String { "\(source.id):\(manga.id)" }
    }

    private var otherSources: [SourceRow] {
        sources.filter { !$0.browseOnly && $0.id != lookupSource?.id }
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                Text("Discover").font(.title.bold())
                if comick != nil {
                    comickShelves
                    Divider().padding(.vertical, 2)
                }
                Text("From a thread").font(.title3.bold())
                Text("Paste a Reddit post and its comments. A link alone cannot be read.")
                    .foregroundStyle(.secondary)
                TextEditor(text: $text)
                    .frame(minHeight: 150)
                    .overlay(RoundedRectangle(cornerRadius: 8).stroke(.secondary.opacity(0.3)))
                Toggle("Use AI", isOn: $useAI)
                if useAI {
                    Text("Requires an API key for an OpenAI-compatible provider. The pasted text is sent to that provider and charges may apply. The key is kept only for this session.")
                        .font(.caption).foregroundStyle(.secondary)
                    TextField("HTTPS endpoint", text: $endpoint)
                    TextField("Model", text: $model)
                    SecureField("API key", text: $key)
                } else {
                    Text("Local pattern matching; no API key required.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Button("Find recommendations") { scan() }
                    .disabled(busy || text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || (useAI && key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty))
                if busy { ProgressView("Reading thread…") }
                if let error { Text(error).foregroundStyle(.red) }
                if searched && recommendations.isEmpty && !busy && error == nil {
                    Text("No recommendations found.").foregroundStyle(.secondary)
                }
                ForEach(recommendations) { recommendation in
                    VStack(alignment: .leading, spacing: 6) {
                        Text(recommendation.title).font(.headline)
                        if let quote = recommendation.quote { Text("“\(quote)”").font(.callout) }
                        if let why = recommendation.why { Text(why).font(.caption).foregroundStyle(.secondary) }
                        if let manga = matches[recommendation.id] {
                            Button("Open \(manga.title)") { onOpen(sourceID, manga) }
                        } else {
                            Button(matching == recommendation.id ? "Searching…" : "Find in selected source") {
                                find(recommendation)
                            }.disabled(matching != nil || sourceID == 0)
                        }
                    }.frame(maxWidth: .infinity, alignment: .leading).padding()
                        .background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
                }
            }.padding()
        }
        .task(id: comick?.id) { await loadComick() }
        .sheet(item: $picked, onDismiss: openPending) { manga in
            #if os(macOS)
            matchResults(for: manga).frame(minWidth: 480, minHeight: 460)
            #else
            matchResults(for: manga)
            #endif
        }
    }

    /// The catalog-only source whose feeds fill the Discover shelves.
    private var comick: SourceRow? { sources.first { $0.name == "Comick" } }

    /// The source a tapped title is looked up in: the selected one when it can
    /// serve chapters, otherwise the first source that can. Comick itself only
    /// publishes catalogue rows.
    private var readingSource: SourceRow? {
        sources.first { $0.id == sourceID && !$0.browseOnly }
            ?? sources.first { !$0.browseOnly }
    }

    private var readingSourceName: String { readingSource?.name ?? "your sources" }

    @ViewBuilder
    private var comickShelves: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline) {
                Text("Comick").font(.title3.bold())
                Spacer(minLength: 12)
                Text("Tap a title to search \(readingSourceName)")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let comickError {
                HStack(spacing: 10) {
                    Text(comickError).font(.callout).foregroundStyle(.secondary)
                    Button("Try Again") { Task { await loadComick() } }
                        .buttonStyle(.glass)
                }
            } else if !comickLoaded {
                ProgressView().controlSize(.small).frame(height: 90)
            } else if comickSections.isEmpty {
                Text("Comick returned no titles.").font(.callout).foregroundStyle(.secondary)
            } else {
                ForEach(comickSections) { section in
                    VStack(alignment: .leading, spacing: 8) {
                        Text(section.title).font(.headline)
                        ScrollView(.horizontal) {
                            LazyHStack(alignment: .top, spacing: 2) {
                                ForEach(section.mangas) { manga in
                                    Button { lookUp(manga) } label: {
                                        CoverCard(manga: manga)
                                            .equatable()
                                            .frame(width: 148)
                                    }
                                    .buttonStyle(CardButtonStyle())
                                }
                            }
                        }
                        .scrollIndicators(.never)
                    }
                }
            }
        }
    }

    private func loadComick() async {
        guard let comick, !comickLoading, !comickLoaded else { return }
        let id = comick.id
        comickLoading = true
        comickError = nil
        do {
            comickSections = try await Task.detached(priority: .userInitiated) {
                try Core.sections(sourceID: id)
            }.value
            comickLoaded = true
        } catch {
            comickError = error.localizedDescription
        }
        comickLoading = false
    }

    /// Comick cannot open a series, so a tapped trend is searched for in the
    /// source that can serve its chapters.
    private func lookUp(_ manga: Manga) {
        lookupSource = readingSource
        searchedOthers = false
        searchText = manga.title
        picked = manga
        search(manga.title, in: lookupSource.map { [$0] } ?? [])
    }

    private func search(_ title: String, in sources: [SourceRow]) {
        let token = UUID()
        lookupID = token
        found = []
        findError = nil
        finding = !sources.isEmpty
        guard finding else {
            findError = "No source can open chapters."
            return
        }
        Task {
            var errors: [String] = []
            for source in sources {
                do {
                    let results = try await Task.detached(priority: .userInitiated) {
                        try Core.search(sourceID: source.id, query: title)
                    }.value
                    guard lookupID == token, picked != nil else { return }
                    var seen: Set<String> = []
                    found.append(contentsOf: results.filter { seen.insert($0.id).inserted }
                        .map { SearchMatch(source: source, manga: $0) })
                    found = ranked(found, for: title)
                } catch {
                    guard lookupID == token, picked != nil else { return }
                    errors.append("\(source.name): \(error.localizedDescription)")
                }
            }
            if !errors.isEmpty { findError = errors.joined(separator: "\n") }
            finding = false
        }
    }

    /// Exact title matches first, so a loose match cannot push the right series
    /// below the fold in a small sheet.
    private func ranked(_ matches: [SearchMatch], for title: String) -> [SearchMatch] {
        let target = title.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)
        func score(_ manga: Manga) -> Int {
            let name = manga.title.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)
            if name == target { return 0 }
            if name.hasPrefix(target) || target.hasPrefix(name) { return 1 }
            if name.contains(target) { return 2 }
            return 3
        }
        return matches.enumerated()
            .sorted { (score($0.element.manga), $0.offset) < (score($1.element.manga), $1.offset) }
            .map(\.element)
    }

    private func runSearch() {
        let term = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !term.isEmpty else { return }
        searchedOthers = false
        search(term, in: lookupSource.map { [$0] } ?? [])
    }

    private func searchOtherSources() {
        let term = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !term.isEmpty, !otherSources.isEmpty else { return }
        searchedOthers = true
        search(term, in: otherSources)
    }

    /// Opening waits for the sheet to finish dismissing; presenting a series
    /// from under a closing sheet drops the zoom transition on iOS.
    private func openPending() {
        lookupID = UUID()
        guard let match = pendingOpen else { return }
        pendingOpen = nil
        onOpen(match.source.id, match.manga)
    }

    private func matchResults(for manga: Manga) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(manga.title).font(.headline).lineLimit(1)
                    let scope = searchedOthers ? "other sources" : (lookupSource?.name ?? "your sources")
                    HStack(spacing: 8) {
                        Text(finding ? "Searching \(scope)…" : "Matches in \(scope)")
                            .font(.caption).foregroundStyle(.secondary)
                            .lineLimit(1)

                        // Below the results grid this was easy to miss, so the
                        // offer sits with the scope line instead.
                        if !found.isEmpty, !searchedOthers, !otherSources.isEmpty, !finding {
                            Button("Other sources", action: searchOtherSources)
                                .font(.caption.weight(.semibold))
                                .buttonStyle(.plain)
                                .foregroundStyle(.tint)
                                .accessibilityLabel("Search other sources")
                        }
                    }
                }
                // The sheet can hand this row less height than it asks for,
                // which clipped the second line.
                .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 12)
                Button("Done") { picked = nil }
            }
            .padding(.horizontal, 16)
            .padding(.top, 16)
            .padding(.bottom, 10)

            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass")
                    .font(.callout)
                    .foregroundStyle(.secondary)

                TextField("Search a different title", text: $searchText)
                    .textFieldStyle(.plain)
                    #if os(iOS)
                    .submitLabel(.search)
                    #endif
                    .onSubmit(runSearch)

                if !searchText.isEmpty {
                    Button {
                        searchText = ""
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                            .foregroundStyle(.tertiary)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Clear the title")
                }

                Button("Search", action: runSearch)
                    .buttonStyle(.glass)
                    .disabled(searchText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 14)

            Divider()
            if found.isEmpty {
                VStack(spacing: 14) {
                    if finding {
                        ProgressView()
                    } else {
                        Text(findError ?? "No results for “\(manga.title)”.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                        if !searchedOthers && !otherSources.isEmpty {
                            Button("Search other sources", action: searchOtherSources)
                                .buttonStyle(.glass)
                        }
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .padding(24)
            } else {
                ScrollView {
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 130), spacing: 6)], spacing: 6) {
                        ForEach(found) { match in
                            Button {
                                pendingOpen = match
                                picked = nil
                            } label: {
                                VStack(alignment: .leading, spacing: 0) {
                                    CoverCard(manga: match.manga).equatable()
                                    Text(match.source.name)
                                        .font(.caption).foregroundStyle(.secondary)
                                        .padding(.horizontal, 8)
                                }
                            }
                            .buttonStyle(CardButtonStyle())
                        }
                    }
                    .padding(14)
                    if finding { ProgressView().padding(.bottom, 14) }
                    if findError != nil {
                        Text("Some sources couldn’t be searched.")
                            .font(.caption).foregroundStyle(.secondary)
                            .padding(.bottom, 14)
                    }
                }
            }
        }
    }

    private func scan() {
        let thread = text
        let ai = useAI ? DiscoverAI(base_url: endpoint, model: model, key: key) : nil
        busy = true
        error = nil
        recommendations = []
        matches = [:]
        searched = true
        Task {
            do {
                recommendations = try await Task.detached(priority: .userInitiated) {
                    try Core.discover(text: thread, ai: ai)
                }.value
            } catch { self.error = error.localizedDescription }
            busy = false
        }
    }

    private func find(_ recommendation: Recommendation) {
        let id = sourceID
        matching = recommendation.id
        error = nil
        Task {
            do {
                let found = try await Task.detached(priority: .userInitiated) {
                    try Core.search(sourceID: id, query: recommendation.title)
                }.value
                if let manga = found.first { matches[recommendation.id] = manga }
                else { error = "No results in the selected source for \(recommendation.title)." }
            } catch { self.error = error.localizedDescription }
            matching = nil
        }
    }
}
