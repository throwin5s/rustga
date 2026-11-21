# rustga
a lightweight, manga/manhwa/manhua reader built in rust. Mihon extensions supported.

## Downloads

Open a series and choose **Download all chapters**, or use **Download** beside
an individual chapter. Download all follows the visible chapter list and its
**Hide duplicate releases** setting. Progress and **Cancel download** appear on
the series screen. Repeating a download resumes saved pages.

The **Downloads** tab lists saved manga for the selected source. Open a manga
there to read completed chapters offline. Downloads are kept in the app's data
directory, separately from the temporary page cache, and are not automatically
evicted. A chapter is marked downloaded only after every page is saved. Cover
images are saved when available; a blocked cover does not prevent chapter downloads.

Downloaded pages also get a bounded display-ready cache to avoid repeated image
decoding. The series screen prepares opening pages in the background (prioritizing
the hovered chapter), and the reader retains recently used page textures. Existing
downloads are prepared automatically when used; no redownload is needed.
