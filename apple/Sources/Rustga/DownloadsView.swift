import SwiftUI

/// The core's saved library for the selected source: one row per series, with
/// the chapters that are already on disk.
struct DownloadsView: View {
    let sourceID: UInt64
    let sourceName: String
    let onOpen: (Manga, Chapter, [Chapter]) -> Void

    @State private var items: [DownloadedManga] = []
    @State private var loading = true
    @State private var failure: String?

    var body: some View {
        ScrollView {
            LazyVStack(spacing: 12) {
                if loading {
                    ProgressView()
                        .frame(maxWidth: .infinity, minHeight: 320)
                } else if let failure {
                    Text(failure)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, minHeight: 320)
                } else if items.isEmpty {
                    empty
                } else {
                    summary
                    ForEach(items) { item in
                        row(item)
                    }
                }
            }
            .padding(.horizontal, 22)
            .padding(.bottom, 22)
        }
        .scrollIndicators(.never)
        #if os(macOS)
        .contentMargins(.top, 78, for: .scrollContent)
        #endif
        .task(id: sourceID) { load() }
    }

    private var empty: some View {
        VStack(spacing: 10) {
            Image(systemName: "arrow.down.circle")
                .font(.system(size: 32, weight: .light))
                .foregroundStyle(.tertiary)
            Text("Nothing saved yet")
                .font(.headline)
            Text("Open a series and press the download button beside a chapter.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: 360)
        .frame(maxWidth: .infinity, minHeight: 320)
    }

    private var summary: some View {
        HStack {
            Text(subtitle)
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
            Button("Refresh") { load() }
                .buttonStyle(.plain)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 4)
        .padding(.bottom, 2)
    }

    private var subtitle: String {
        let series = items.count == 1 ? "1 series" : "\(items.count) series"
        let chapters = items.reduce(0) { $0 + $1.chapters.count }
        let saved = chapters == 1 ? "1 chapter" : "\(chapters) chapters"
        return sourceName.isEmpty ? "\(series) · \(saved)" : "\(series) · \(saved) · \(sourceName)"
    }

    private func row(_ item: DownloadedManga) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            // The library appends each completed download, so the last chapter
            // is the most recent one; the chips below choose any saved chapter.
            Button {
                if let recent = item.chapters.last {
                    onOpen(item.manga, recent, readingOrder(item.chapters))
                }
            } label: {
                HStack(alignment: .top, spacing: 14) {
                    CachedCover(url: item.manga.thumbnailURL.flatMap { URL(string: $0) })
                    .frame(width: 56, height: 80)
                    .clipShape(RoundedRectangle(cornerRadius: 8))

                    VStack(alignment: .leading, spacing: 5) {
                        Text(item.manga.title)
                            .font(.callout.weight(.medium))
                            .lineLimit(2)

                        Text(item.chapters.count == 1 ? "1 chapter saved" : "\(item.chapters.count) chapters saved")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }

                    Spacer(minLength: 0)

                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.tertiary)
                        .padding(.top, 2)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help("Read the most recent download")

            ScrollView(.horizontal) {
                HStack(spacing: 6) {
                    ForEach(item.chapters) { chapter in
                        let name = chapter.name.isEmpty ? "Chapter" : chapter.name
                        Button {
                            onOpen(item.manga, chapter, readingOrder(item.chapters))
                        } label: {
                            Text(name)
                                .font(.caption)
                                .lineLimit(1)
                                .padding(.horizontal, 9)
                                .padding(.vertical, 4)
                                .background(Color.primary.opacity(0.08), in: Capsule())
                        }
                        .buttonStyle(.plain)
                        .help("Read \(name)")
                    }
                }
                .padding(.horizontal, 2)
            }
            .scrollIndicators(.never)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.secondary.opacity(0.09), in: RoundedRectangle(cornerRadius: 12))
    }

    /// Saved chapters are appended as they are downloaded; the reader needs
    /// them newest first so it can step to the following release.
    private func readingOrder(_ chapters: [Chapter]) -> [Chapter] {
        chapters.sorted {
            let left = $0.number ?? -Float.greatestFiniteMagnitude
            let right = $1.number ?? -Float.greatestFiniteMagnitude
            if left != right { return left > right }
            return $0.url < $1.url
        }
    }

    private func load() {
        guard sourceID != 0 else {
            items = []
            loading = false
            return
        }
        let sourceID = self.sourceID
        loading = true
        failure = nil
        Task {
            do {
                items = try await Task.detached(priority: .utility) {
                    try Core.downloads(sourceID: sourceID)
                }.value
            } catch let error {
                items = []
                failure = error.localizedDescription
            }
            loading = false
        }
    }
}
