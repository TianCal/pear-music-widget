# Kugeci lyrics fallback

Verified against first-party pages on 2026-10-03. No documented API was found;
the public HTML is sufficient for a conservative automatic fallback.

## Observed routes and formats

- The [supplied song](https://www.kugeci.com/song/9QbM6b0K) identifies **TizzyT — 月半小夜曲** in its title, `h1`, and `/singer/` link. Its server-rendered `div#lyricsContainer` contains LRC timestamps with HTML `br` separators. Decode HTML entities and convert breaks to newlines before calling `parse_lrc`. A separate `#txt` tab repeats the text without timestamps; do not concatenate both tabs. This page needs neither JavaScript nor authentication to retrieve its lyrics.
- The site's form submits `GET /search?q=...`. [Title-only search](https://www.kugeci.com/search?q=%E6%9C%88%E5%8D%8A%E5%B0%8F%E5%A4%9C%E6%9B%B2) finds the supplied song among different performers, duplicate recordings, and live versions. Within `table#tablesort tbody`, each row's second `td` holds the song title and `/song/<id>` link; the third holds one or more `/singer/<id>` links. A later cell repeats the song link. Footer recommendations contain unrelated song links even when there are no results, so scope parsing to the result table.
- [Combined title and artist search](https://www.kugeci.com/search?q=%E6%9C%88%E5%8D%8A%E5%B0%8F%E5%A4%9C%E6%9B%B2+TizzyT) returned no song results. [Artist-only search](https://www.kugeci.com/search?q=TizzyT) returns a singer entry rather than that singer's songs. Search by title and filter rows locally by artist.
- Search does not automatically normalize Chinese scripts: [愛情轉移](https://www.kugeci.com/search?q=%E6%84%9B%E6%83%85%E8%BD%89%E7%A7%BB) returned no results, whereas [爱情转移](https://www.kugeci.com/search?q=%E7%88%B1%E6%83%85%E8%BD%AC%E7%A7%BB) returned matching titles. [愛情](https://www.kugeci.com/search?q=%E6%84%9B%E6%83%85) also finds titles written in traditional characters, so originals and simplified variants both have value. Normalize title and artist for comparisons, independently of the display setting.
- [慕容雪 search](https://www.kugeci.com/search?q=%E6%85%95%E5%AE%B9%E9%9B%AA) has collaboration rows with two singers and distinguishes plain, `(Live)`, and `(Live版)` titles. The search table offers no duration or album metadata. Do not infer recording duration from the final timestamp: the supplied page includes production credits near its end.
- Curl worked with the app's `USER_AGENT` as well as curl's default agent. Pages include Cloudflare JavaScript detection code, but these checks did not prevent the observed requests. No stable availability guarantee follows from this sample.
- [robots.txt](https://www.kugeci.com/robots.txt) disallows `/download/`, `/storage/`, and `/index.php/` for general agents. `/search` and `/song` are not in those disallow rules. No download route is necessary for this integration; robots rules do not establish a reuse license.

## Integration judgment

Keep requests in `lyrics.rs`, with finite timeouts, bounded HTML size, same-host
song URLs, and existing caching. Add Kugeci after current matching tiers, allowing
its synced result to replace an unsynced fallback. Require matching normalized
title and artist in both search rows and the fetched song page. Preserve recording
qualifiers during matching so live recordings cannot silently select a studio
version. Treat changed markup, failed requests, and empty lyrics as misses.
Register `kugeci` with `how_from` for disk-cache source labels.

The [周旋 page](https://www.kugeci.com/song/7CJI2dO0) lists 王以太 and 艾热 AIR
as separate singers. The player's credit `王以太和艾热 AIR` must match that list
as a collaboration. Passing only a lead artist or comparing the combined string
to a single singer misses this track; preserve the full credit for Kugeci.

This is HTML extraction, so parser fixtures should cover unrelated footer links,
collaborations, duplicate links, entity decoding, Chinese normalization, and
version mismatches. The site exposes no album/duration matching contract; when
several identical title/artist records remain, recording identity can be ambiguous.

## Verified matching corrections

The live queue audit exposed provider aliases and uploader metadata. The four
user-supplied pages were checked directly and the production lookup now resolves
all four: [台北一夜](https://www.kugeci.com/song/J1G8OmUF),
[很高兴认识你](https://www.kugeci.com/song/Dm83A35v),
[爱的回归线](https://www.kugeci.com/song/nGQE7U7t), and
[答案](https://www.kugeci.com/song/kEJQWv9l).

台北一夜 credits 华云龙KLE where the queue credits 华云龙; this is a specific
verified alias. Official-MV, lyric-video, and CCTV song/performer title formats
supply explicit credits when the artist field is an uploader. Artist and title
verification still applies to both the search result and fetched page. 迴 and 回
normalize for matching/search. Collaboration segmentation uses the provider's
whole names, keeping conjunction characters inside names intact.
