import ImageIO
import SwiftUI
import CryptoKit

actor PageCache {
    static let shared = PageCache()
    private let cache = URLCache(memoryCapacity: 16 * 1024 * 1024, diskCapacity: 256 * 1024 * 1024, diskPath: "rustga-reader-pages")
    private var generation = 0
    /// A page can be requested by its visible card and by the prefetcher at
    /// the same time. Both wait on the same core request instead of downloading
    /// the bytes twice.
    private var inFlight: [String: Task<Data, Error>] = [:]

    func data(sourceID: UInt64, page: Page) async throws -> Data {
        let key = "\(sourceID):\(page.imageURL ?? page.url)"
        let digest = SHA256.hash(data: Data(key.utf8)).map { String(format: "%02x", $0) }.joined()
        let url = URL(string: "https://cache.invalid/\(digest)")!
        let request = URLRequest(url: url)
        if let cached = cache.cachedResponse(for: request) { return cached.data }
        if let pending = inFlight[key] { return try await pending.value }

        let currentGeneration = generation
        let load = Task.detached(priority: .userInitiated) {
            try Core.image(sourceID: sourceID, page: page)
        }
        inFlight[key] = load
        do {
            let data = try await load.value
            inFlight[key] = nil
            if generation == currentGeneration {
                let response = URLResponse(url: url, mimeType: "application/octet-stream", expectedContentLength: data.count, textEncodingName: nil)
                cache.storeCachedResponse(CachedURLResponse(response: response, data: data), for: request)
            }
            return data
        } catch {
            inFlight[key] = nil
            throw error
        }
    }

    func clear() {
        generation += 1
        for task in inFlight.values { task.cancel() }
        inFlight.removeAll()
        cache.removeAllCachedResponses()
    }
}

private enum PageImageError: LocalizedError {
    case notAnImage

    var errorDescription: String? { "not an image" }
}

private enum PageDecoder {
    nonisolated static func downsample(_ data: Data, maxPixel: Int = 1600) -> CGImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixel,
            kCGImageSourceShouldCacheImmediately: true,
        ]
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else {
            return nil
        }
        return normalized(image)
    }

    /// Page files come from many different scanlation tools. Drawing each one
    /// into sRGB gives every page the same color handling on its first frame,
    /// including CMYK and untagged JPEGs.
    nonisolated private static func normalized(_ image: CGImage) -> CGImage {
        guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB) else { return image }
        let width = image.width
        let height = image.height
        guard width > 0, height > 0,
              let context = CGContext(
                  data: nil,
                  width: width,
                  height: height,
                  bitsPerComponent: 8,
                  bytesPerRow: 0,
                  space: colorSpace,
                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
              ) else { return image }
        context.interpolationQuality = .high
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        return context.makeImage() ?? image
    }
}

/// Decoded pages shared by the visible reader and its look-ahead prefetcher.
/// The byte cache alone still made every new card decode on the main thread's
/// critical path; this keeps the next few pages ready as images.
@MainActor
final class PageImages {
    static let shared = PageImages()

    private let images = NSCache<NSString, CGImage>()
    private var tasks: [String: Entry] = [:]

    private struct Entry {
        let id: UUID
        let task: Task<CGImage, Error>
    }

    private init() {
        images.totalCostLimit = 128 * 1024 * 1024
        images.countLimit = 16
    }

    func cachedImage(sourceID: UInt64, page: Page) -> CGImage? {
        images.object(forKey: Self.key(sourceID: sourceID, page: page) as NSString)
    }

    func image(sourceID: UInt64, page: Page) async throws -> CGImage {
        let key = Self.key(sourceID: sourceID, page: page)
        if let cached = images.object(forKey: key as NSString) { return cached }
        if let pending = tasks[key] { return try await pending.task.value }
        return try await makeTask(sourceID: sourceID, page: page, key: key, priority: .userInitiated).value
    }

    /// Starts the next pages ahead of the reader. Tasks coalesce with the
    /// visible card, and the cap keeps fast scrolling from queuing an entire
    /// chapter of decodes while still leaving room for the next chapter's
    /// first pages.
    func prefetch(sourceID: UInt64, pages: [Page]) {
        for page in pages {
            guard tasks.count < 12 else { break }
            let key = Self.key(sourceID: sourceID, page: page)
            if images.object(forKey: key as NSString) != nil || tasks[key] != nil { continue }
            _ = makeTask(sourceID: sourceID, page: page, key: key, priority: .utility)
        }
    }

    func clear() {
        for entry in tasks.values { entry.task.cancel() }
        tasks.removeAll()
        images.removeAllObjects()
    }

    private func makeTask(
        sourceID: UInt64,
        page: Page,
        key: String,
        priority: TaskPriority
    ) -> Task<CGImage, Error> {
        let id = UUID()
        let task = Task(priority: priority) { [weak self] in
            defer {
                if let self, self.tasks[key]?.id == id {
                    self.tasks[key] = nil
                }
            }

            let data = try await PageCache.shared.data(sourceID: sourceID, page: page)
            try Task.checkCancellation()
            let decoded = await Task.detached(priority: .userInitiated) {
                PageDecoder.downsample(data)
            }.value
            try Task.checkCancellation()
            guard let decoded else { throw PageImageError.notAnImage }
            self?.images.setObject(decoded, forKey: key as NSString, cost: decoded.bytesPerRow * decoded.height)
            return decoded
        }
        tasks[key] = Entry(id: id, task: task)
        return task
    }

    private static func key(sourceID: UInt64, page: Page) -> String {
        "\(sourceID):\(page.id)"
    }
}

/// Vertical reader: page images fetched one at a time through the byte buffer.
struct ReaderView: View {
    let sourceID: UInt64
    /// The series' chapters in reading order, newest first.
    let chapters: [Chapter]
    let onBack: () -> Void
    let onProgress: (Chapter, Int, Bool) -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var chapter: Chapter
    /// The chapter whose pages are actually in the scroll view. It trails
    /// `chapter` while the next chapter is being fetched.
    @State private var displayedChapter: Chapter
    @State private var pages: [Page] = []
    /// Page lists fetched ahead of time for the chapter that follows this one.
    @State private var preparedPages: [String: [Page]] = [:]
    @State private var message: String?
    @State private var loading = true
    /// A next chapter is loading while the current pages stay readable.
    @State private var switching = false
    /// The chapter bar floats over the pages and follows the scroll direction.
    @State private var showHeader = true
    /// Scroll distance since the bar last moved, so slow trackpad scrolling
    /// still counts and jitter cannot flip the bar back and forth.
    @State private var scrollTravel: CGFloat = 0
    /// A chapter that opens shorter than the screen has nowhere to scroll, so
    /// its end control stays a tap instead of firing automatically.
    @State private var didScroll = false
    @State private var advancing = false
    @State private var resume: ReadingProgress?
    @State private var visiblePage: String?
    @State private var userScrolling = false
    @State private var pendingPage: String?
    @State private var nearEnd = false
    @State private var lastPageReady = false
    @State private var finishedChapter = false
    /// Position inside the page slider while its thumb is held.
    @State private var scrub = 0.0
    @State private var scrubbing = false
    /// The next chapter failed to open; the current pages stay readable.
    @State private var nextFailure: String?

    init(chapter: Chapter, sourceID: UInt64, chapters: [Chapter], resume: ReadingProgress?,
         onProgress: @escaping (Chapter, Int, Bool) -> Void, onBack: @escaping () -> Void) {
        self.sourceID = sourceID
        self.chapters = chapters
        self.onBack = onBack
        self.onProgress = onProgress
        _resume = State(initialValue: resume)
        _chapter = State(initialValue: chapter)
        _displayedChapter = State(initialValue: chapter)
    }

    /// The next chapter is one row earlier because the list runs newest first.
    private var nextChapter: Chapter? {
        guard let index = chapters.firstIndex(where: { $0.url == chapter.url }), index > 0 else {
            return nil
        }
        return chapters[index - 1]
    }

    var body: some View {
        ZStack(alignment: .top) {
            content
                .contentShape(Rectangle())
                .simultaneousGesture(TapGesture().onEnded { toggleHeader() })

            if showHeader {
                header
                    .transition(.move(edge: .top).combined(with: .opacity))
                    .zIndex(1)
            }

            VStack(spacing: 10) {
                Spacer()
                if let nextFailure {
                    failureBar(nextFailure)
                }
                if showHeader && pages.count > 1 {
                    scrubber
                }
            }
            .animation(reduceMotion ? nil : .easeInOut(duration: 0.18), value: showHeader)
            .animation(reduceMotion ? nil : .easeInOut(duration: 0.18), value: nextFailure)
            .zIndex(2)
        }
        .task(id: chapter.id) { await load() }
        .onDisappear { saveScrollProgress() }
    }

    @ViewBuilder
    private var content: some View {
        if loading && pages.isEmpty {
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let message, pages.isEmpty {
            Text(message)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            ZStack(alignment: .bottom) {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(pages) { page in
                            PageImage(sourceID: sourceID, page: page, isLast: page == pages.last) { visible in
                                prefetch(after: visible)
                            }
                        }
                    }
                    .scrollTargetLayout()
                }
                .onPreferenceChange(ReadyReaderPage.self) { ready in
                    lastPageReady = ready != nil && ready == pages.last?.id
                }
                .scrollPosition(id: $visiblePage, anchor: .top)
                .scrollIndicators(.never)
                .onScrollPhaseChange { _, phase in
                    let scrolling = phase == .interacting || phase == .decelerating
                    if userScrolling && !scrolling {
                        finishIfAtEnd()
                        if !finishedChapter || !nearEnd { pendingPage = visiblePage ?? pendingPage }
                        saveScrollProgress()
                    }
                    userScrolling = scrolling
                }
                .onChange(of: visiblePage) { _, id in
                    if userScrolling, !finishedChapter || !nearEnd, let id { pendingPage = id }
                }
                .id(displayedChapter.id)
                .onScrollGeometryChange(for: ScrollMetrics.self) { geometry in
                    ScrollMetrics(
                        offset: geometry.contentOffset.y,
                        remaining: geometry.contentSize.height - geometry.containerSize.height - geometry.contentOffset.y
                    )
                } action: { old, new in
                    let atEnd = new.remaining <= 24
                    if nearEnd != atEnd { nearEnd = atEnd }
                    if userScrolling { scanScroll(new.offset - old.offset) }
                    if atEnd { finishIfAtEnd() }
                }

                if switching {
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text("Loading next chapter…")
                            .font(.callout)
                    }
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .background(.regularMaterial, in: Capsule())
                    .overlay {
                        Capsule().strokeBorder(.white.opacity(0.08))
                    }
                    .shadow(color: .black.opacity(0.25), radius: 12, y: 5)
                    .padding(.bottom, 24)
                    .transition(.opacity)
                    .allowsHitTesting(false)
                }
            }
            .animation(reduceMotion ? nil : .easeInOut(duration: 0.18), value: switching)
        }
    }

    private func saveScrollProgress() {
        guard let id = pendingPage else { return }
        pendingPage = nil
        guard let page = pages.first(where: { $0.id == id }) else { return }
        onProgress(displayedChapter, page.index, false)
    }

    private var currentPage: Int {
        pages.first { $0.id == visiblePage }?.index ?? 0
    }

    /// Page slider shown with the chapter bar. The jump happens when the thumb
    /// is released, so dragging through a long chapter does not queue a scroll
    /// for every intermediate page.
    private var scrubber: some View {
        HStack(spacing: 12) {
            Text("\(displayedPageNumber) / \(pages.count)")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(minWidth: 54, alignment: .leading)

            Slider(
                value: Binding(
                    get: { scrubbing ? scrub : Double(currentPage) },
                    set: { scrub = $0 }
                ),
                in: 0...Double(max(pages.count - 1, 0)),
                step: 1,
                onEditingChanged: { editing in
                    if editing {
                        scrub = Double(currentPage)
                        scrubbing = true
                    } else {
                        scrubbing = false
                        jump(to: Int(scrub))
                    }
                }
            )
            .frame(maxWidth: 420)
            .accessibilityLabel("Page")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
        .background(.regularMaterial, in: Capsule())
        .overlay {
            Capsule().strokeBorder(.white.opacity(0.08))
        }
        .shadow(color: .black.opacity(0.25), radius: 12, y: 5)
        .padding(.horizontal, 22)
        .padding(.bottom, 22)
        .transition(.opacity)
    }

    private var displayedPageNumber: Int {
        let index = scrubbing ? Int(scrub) : currentPage
        return min(max(index + 1, 1), max(pages.count, 1))
    }

    private func jump(to index: Int) {
        guard pages.indices.contains(index) else { return }
        userScrolling = false
        pendingPage = nil
        visiblePage = pages[index].id
        onProgress(displayedChapter, pages[index].index, false)
        PageImages.shared.prefetch(sourceID: sourceID, pages: Array(pages[index...].prefix(3)))
    }

    private func failureBar(_ message: String) -> some View {
        HStack(spacing: 12) {
            Text(message)
                .font(.callout)
                .lineLimit(2)

            Button("Retry") {
                nextFailure = nil
                if let next = nextChapter { open(next) }
            }
            .buttonStyle(.glass)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(.regularMaterial, in: Capsule())
        .shadow(color: .black.opacity(0.25), radius: 12, y: 5)
        .padding(.horizontal, 22)
    }

    private func toggleHeader() {
        scrollTravel = 0
        withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.2)) {
            showHeader.toggle()
        }
    }

    /// Scrolling only hides the bar. It never brings it back; a tap is the
    /// only way to reveal it.
    private func scanScroll(_ delta: CGFloat) {
        guard delta != 0 else { return }
        if !didScroll, abs(delta) >= 1 {
            didScroll = true
        }
        guard showHeader else { return }
        if delta > 0 {
            scrollTravel += delta
            if scrollTravel >= 8 {
                scrollTravel = 0
                setHeader(shown: false)
            }
        } else {
            scrollTravel = 0
        }
    }

    private func setHeader(shown: Bool) {
        guard showHeader != shown else { return }
        withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.2)) {
            showHeader = shown
        }
    }

    private func finishIfAtEnd() {
        guard nearEnd, lastPageReady, let last = pages.last else { return }
        if !finishedChapter {
            finishedChapter = true
            pendingPage = nil
            onProgress(displayedChapter, last.index, true)
        }
        if userScrolling, didScroll, !advancing, !loading, nextFailure == nil, let next = nextChapter {
            open(next)
        }
    }

    private func open(_ next: Chapter) {
        guard !advancing else { return }
        advancing = true
        scrollTravel = 0
        if preparedPages[next.id] == nil && !pages.isEmpty {
            switching = true
        }
        chapter = next
    }

    private func prefetch(after page: Page) {
        guard let index = pages.firstIndex(where: { $0.id == page.id }) else { return }
        let start = index + 1
        let end = min(start + 3, pages.count)
        guard start < end else { return }
        PageImages.shared.prefetch(sourceID: sourceID, pages: Array(pages[start..<end]))
    }


    private var header: some View {
        HStack(spacing: 12) {
            Button(action: onBack) {
                Image(systemName: "chevron.left")
                    .font(.body.weight(.semibold))
            }
            .buttonStyle(.glass)
            .help("Back to chapters")

            Text(chapter.name.isEmpty ? "Chapter" : chapter.name)
                .font(.headline)
                .lineLimit(1)

            Spacer()

            if let next = nextChapter {
                Button { open(next) } label: {
                    Image(systemName: "chevron.right")
                        .font(.body.weight(.semibold))
                }
                .buttonStyle(.glass)
                .disabled(advancing)
                .help("Next chapter")
                .accessibilityLabel("Next chapter")
            }

            if !pages.isEmpty && !switching {
                Text("\(pages.count) pages")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 12)
        .background(.bar)
    }

    private func load() async {
        let requested = chapter
        let sourceID = self.sourceID
        let hasPages = !pages.isEmpty
        guard !hasPages || requested.id != displayedChapter.id else { return }
        if !hasPages {
            loading = true
        }
        message = nil
        didScroll = false
        if hasPages && preparedPages[requested.id] == nil {
            switching = true
        }

        do {
            let loaded: [Page]
            if let ready = preparedPages[requested.id] {
                loaded = ready
            } else {
                loaded = try await Task.detached(priority: .userInitiated) {
                    try Core.pages(sourceID: sourceID, chapter: requested)
                }.value
            }
            guard !Task.isCancelled, chapter.id == requested.id else { return }
            saveScrollProgress()
            nearEnd = false
            lastPageReady = false
            finishedChapter = false
            pages = loaded
            preparedPages[requested.id] = loaded
            displayedChapter = requested
            userScrolling = false
            let startingPage = resume?.chapter.id == requested.id ? resume?.page(in: loaded) : loaded.first
            resume = nil
            visiblePage = startingPage?.id
            message = loaded.isEmpty ? "This chapter has no pages." : nil
            nextFailure = nil
            if let startingPage {
                onProgress(requested, startingPage.index, false)
                if let start = loaded.firstIndex(of: startingPage) {
                    PageImages.shared.prefetch(sourceID: sourceID, pages: Array(loaded[start...].prefix(4)))
                }
            }
            if let next = nextChapter {
                prepare(next)
            }
        } catch {
            guard !Task.isCancelled, chapter.id == requested.id else { return }
            if pages.isEmpty {
                message = error.localizedDescription
            } else {
                // The next chapter failed to load; keep reading the one that is
                // already on screen instead of turning the reader blank.
                chapter = displayedChapter
                nextFailure = "Couldn’t open the next chapter."
            }
        }
        advancing = false
        loading = false
        switching = false
    }

    /// Fetches the next chapter's page list and first images while the reader
    /// is still on the current chapter. The page list is small; the expensive
    /// part is the first page images, so those are decoded ahead too.
    private func prepare(_ next: Chapter) {
        guard preparedPages[next.id] == nil else { return }
        let sourceID = self.sourceID
        Task {
            do {
                let prepared = try await Task.detached(priority: .utility) {
                    try Core.pages(sourceID: sourceID, chapter: next)
                }.value
                guard !Task.isCancelled else { return }
                preparedPages[next.id] = prepared
                PageImages.shared.prefetch(sourceID: sourceID, pages: Array(prepared.prefix(4)))
            } catch {
                // The foreground load reports the failure if the reader reaches it.
            }
        }
    }

    private struct ScrollMetrics: Equatable {
        let offset: CGFloat
        let remaining: CGFloat
    }
}

private struct ReadyReaderPage: PreferenceKey {
    static let defaultValue: String? = nil

    static func reduce(value: inout String?, nextValue: () -> String?) {
        value = nextValue() ?? value
    }
}

/// One page: bytes from the core, decoded at a bounded size so a long chapter
/// cannot hold every page at full resolution.
private struct PageImage: View {
    let sourceID: UInt64
    let page: Page
    let isLast: Bool
    let onVisible: (Page) -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var image: CGImage?
    @State private var failed: String?
    @State private var attempt = 0

    var body: some View {
        Group {
            if let image {
                Image(decorative: image, scale: 1)
                    .resizable()
                    .scaledToFit()
                    .transition(.opacity)
            } else if let failed {
                VStack(spacing: 8) {
                    Text("Page \(page.index + 1): \(failed)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Button("Retry") { attempt += 1 }
                        .buttonStyle(.glass)
                }
                // The chapter bar floats over the top of the page, so keep the
                // message and its button in the lower half of the gap.
                .frame(maxWidth: .infinity, minHeight: 130, alignment: .bottom)
                .padding(.bottom, 10)
            } else {
                ProgressView()
                    .frame(maxWidth: .infinity, minHeight: 180)
            }
        }
        .frame(maxWidth: .infinity)
        .preference(key: ReadyReaderPage.self, value: isLast && image != nil ? page.id : nil)
        .onAppear {
            if let cached = PageImages.shared.cachedImage(sourceID: sourceID, page: page) {
                image = cached
            }
            onVisible(page)
        }
        .task(id: "\(page.id)#\(attempt)") { await load() }
    }

    private func load() async {
        failed = nil
        if let cached = PageImages.shared.cachedImage(sourceID: sourceID, page: page) {
            image = cached
            return
        }
        do {
            let decoded = try await PageImages.shared.image(sourceID: sourceID, page: page)
            guard !Task.isCancelled else { return }
            withAnimation(reduceMotion ? nil : .easeIn(duration: 0.2)) {
                image = decoded
            }
        } catch {
            guard !Task.isCancelled else { return }
            failed = error.localizedDescription
        }
    }
}
