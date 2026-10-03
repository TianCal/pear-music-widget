# Current queue lyrics audit — 2026-10-03

Audit of the first 50 tracks in the queue snapshot captured for this request. The app’s real lookup flow used track title, artist, album, and duration. Lookups were fresh with disk caching disabled, at most three tracks ran concurrently, and playback was not advanced. Remaining misses reproduced across audit runs.

## Result

- 45 tracks returned synced lyrics: 34 YouTube Music, 3 LRCLib, 8 Kugeci.
- 2 tracks are marked instrumental by LRCLib.
- 3 tracks still returned no accepted lyric source. This means the current matching flow found no usable result, not that lyrics cannot exist elsewhere.

Both `月半小夜曲 (你怎么不回答)` / `Tizzy T` and the official-video upload of 无赖 resolve to Kugeci. Recording-version labels such as Live, remix, and 粤语版 remain part of matching.

## Four reported failures — verified fixed

| Queue position | Track and supplied source | Synced lines returned | Fix |
| --- | --- | --- | --- |
| 13 | [台北一夜](https://www.kugeci.com/song/J1G8OmUF) | 80 | Verified 华云龙 / 华云龙KLE alias. |
| 26 | [很高兴认识你](https://www.kugeci.com/song/Dm83A35v) | 133 | Extract C-BLOCK and song title before the official MV marker; ignore sponsor text. |
| 40 | [爱的回归线](https://www.kugeci.com/song/nGQE7U7t) | 42 | Extract 陈韵若 and song title from the lyric-video format; normalize 迴 / 回. |
| 48 | [答案](https://www.kugeci.com/song/kEJQWv9l) | 44 | Extract song title and both performers from the CCTV title. |

Artists and titles extracted from video credits are checked against both the search row and fetched song page. The 华云龙 alias is specific; arbitrary artist suffixes are not discarded. Collaboration matching segments the site’s actual artist names rather than splitting inside names such as 楊和蘇.

## Remaining lyric-bearing misses

| Queue position | Title | Artist | Finding |
| --- | --- | --- | --- |
| 3 | 甲乙丙丁Strangers (粤语版) | 李佳薇 | The requested Cantonese-version title has no accepted match. A related 李佳薇 page has not been verified as that version. |
| 15 | 海棠果&孔雀 - 最好的我 (50 Feet) 「只是你眼眸 走漏了一種，Baby baby 想愛不能愛的哀愁」【動態歌詞/Lyrics】 | ST Music Video Channel | Uploader credit and a decorated multi-performer lyric-video title; no accepted match. |
| 16 | 《Y + Angel + If You Love Me + 不得不爱+ Off The Hook》 最近在抖音很火的小混搭完整版来了 | 每天都要开心🪿 | A mashup credited to an uploader; no accepted match for the combined recording. |

## Instrumental evidence

- [LRCLib: Can You Hear The Music / Ludwig Göransson](https://lrclib.net/api/search?q=Can%20You%20Hear%20The%20Music%20Ludwig%20G%C3%B6ransson): matching records are marked instrumental.
- [LRCLib: Luv (sic) pt6 Uyama Hiroto Remix Instrumentals / Nujabes](https://lrclib.net/api/search?q=Luv%20%28sic%29%20pt6%20Uyama%20Hiroto%20Remix%20Instrumentals%20Nujabes): matching records are marked instrumental.

## All 50 tracks

| Position | Track | Artist | Result | Source |
| --- | --- | --- | --- | --- |
| 1 | [染缸](https://music.youtube.com/watch?v=bKrIeoPnQLQ) | 楊和蘇KeyNG和JinJiBeWater_隼 | Synced | YouTube Music |
| 2 | [月半小夜曲 (你怎么不回答)](https://music.youtube.com/watch?v=SQFm8DEi1e4) | Tizzy T | Synced | Kugeci |
| 3 | [甲乙丙丁Strangers (粤语版)](https://music.youtube.com/watch?v=cjTdZMOWrVI) | 李佳薇 | Unresolved | — |
| 4 | [获奖人](https://music.youtube.com/watch?v=nAouZ2cYGts) | 李荣浩 | Synced | Kugeci |
| 5 | [甲乙丙丁Strangers](https://music.youtube.com/watch?v=YhiQDARUJ7Y) | 李佳薇 | Synced | YouTube Music |
| 6 | [玻璃 demo](https://music.youtube.com/watch?v=IqK_Ovh277k) | Gareth.T | Synced | YouTube Music |
| 7 | [紧急联络人](https://music.youtube.com/watch?v=s_MKpsZ9YBQ) | Gareth.T | Synced | YouTube Music |
| 8 | [用背脊唱情歌](https://music.youtube.com/watch?v=hzHWXQtwBa4) | Gareth.T | Synced | YouTube Music |
| 9 | [跟悲伤结了帐](https://music.youtube.com/watch?v=EOkzJ6tzP0g) | Gareth.T和揽佬SKAI ISYOURGOD | Synced | YouTube Music |
| 10 | [單程票](https://music.youtube.com/watch?v=jQbCnQ_JQx0) | 派偉俊 | Synced | YouTube Music |
| 11 | [我偏要（《双轨》影视剧主题曲）](https://music.youtube.com/watch?v=zNXk4MFkITo) | 张碧晨 | Synced | YouTube Music |
| 12 | [一半一半](https://music.youtube.com/watch?v=PfuBpbIR638) | Top Barry和INDEcompany | Synced | LRCLib (exact) |
| 13 | [台北一夜](https://music.youtube.com/watch?v=JKBuWjPijEE) | Vansdaddy和华云龙 | Synced | Kugeci |
| 14 | [周旋](https://music.youtube.com/watch?v=6WPWpadfJfI) | 王以太和艾热 AIR | Synced | Kugeci |
| 15 | [海棠果&孔雀 - 最好的我 (50 Feet) 「只是你眼眸 走漏了一種，Baby baby 想愛不能愛的哀愁」【動態歌詞/Lyrics】](https://music.youtube.com/watch?v=sMwH-wE-kz4) | ST Music Video Channel | Unresolved | — |
| 16 | [《Y + Angel + If You Love Me + 不得不爱+ Off The Hook》 最近在抖音很火的小混搭完整版来了](https://music.youtube.com/watch?v=7wMkOeLYUGE) | 每天都要开心🪿 | Unresolved | — |
| 17 | [怨偶 feat. 艾怡良 Eve Ai（合作音乐人：艾怡良 Eve Ai）](https://music.youtube.com/watch?v=MrQC85xV3_o) | MC HotDog熱狗/艾怡良 | Synced | YouTube Music |
| 18 | [Angel](https://music.youtube.com/watch?v=sXP4uMm0teA) | Yoonmirae with Tiger JK & Bizzy | Synced | YouTube Music |
| 19 | [Satisfied](https://music.youtube.com/watch?v=InupuylYdcY) | Original Broadway Cast of Hamilton和Renée Elise Goldsberry | Synced | YouTube Music |
| 20 | [操場酒吧](https://music.youtube.com/watch?v=xgcILxHKla4) | 張震嶽 | Synced | YouTube Music |
| 21 | [Off The Hook](https://music.youtube.com/watch?v=hR51yCq5oco) | Jeff Jarvis | Synced | LRCLib (search) |
| 22 | [天若有情 ((电视剧「锦绣未央」主题曲)](https://music.youtube.com/watch?v=ryoRYNk2x10) | A-Lin | Synced | YouTube Music |
| 23 | [BLAME ON ME](https://music.youtube.com/watch?v=jJyxh2VvDVo) | Gummy B | Synced | YouTube Music |
| 24 | [Ditto](https://music.youtube.com/watch?v=V6TEcoNUmc8) | NewJeans | Synced | YouTube Music |
| 25 | [DAY1 (2020)](https://music.youtube.com/watch?v=GtM5h9PrlxE) | 艾志恒Asen | Synced | YouTube Music |
| 26 | [🏍C-BLOCK : 很高兴认识你  🛵【 OFFICIAL MV 】Sup Music X 陌陌  "送给每个美好的相遇"](https://music.youtube.com/watch?v=r6GIkzj2bKc) | ZHONG.TV | Synced | Kugeci |
| 27 | [功夫胖 KUNGFU-PEN ：「无赖」🐼 🐼 🐼 【 OFFICIAL MV  】](https://music.youtube.com/watch?v=g7-V9JuNXuI) | ZHONG.TV | Synced | Kugeci |
| 28 | [左轉燈 (1000 Times+1)（合作音乐人：mac ova seas）](https://music.youtube.com/watch?v=kUZZIBciO6A) | 派偉俊 | Synced | YouTube Music |
| 29 | [山脚](https://music.youtube.com/watch?v=afCKnKcu9o8) | Jony J | Synced | YouTube Music |
| 30 | [不稱職的天才](https://music.youtube.com/watch?v=Ax4IKl4lOU8) | 王以太 | Synced | YouTube Music |
| 31 | [미워 (Ego)](https://music.youtube.com/watch?v=GWaW24LL0tM) | Crush | Synced | YouTube Music |
| 32 | [Nothing To Say](https://music.youtube.com/watch?v=az7cnBm62bM) | 方大同 | Synced | YouTube Music |
| 33 | [到此為止](https://music.youtube.com/watch?v=oJ0DlOY9vMI) | 徐佳瑩 | Synced | YouTube Music |
| 34 | [就讓這首歌](https://music.youtube.com/watch?v=DuwpK34ziq4) | 張震嶽+Featuring：MC HotDog ＆ Patty Hou | Synced | YouTube Music |
| 35 | [名字](https://music.youtube.com/watch?v=ZgsA6-AAlDA) | 李荣浩 | Synced | YouTube Music |
| 36 | [空中飛人](https://music.youtube.com/watch?v=Fs8HGtRsrJA) | 竇靖童 | Synced | YouTube Music |
| 37 | [反方向的鐘](https://music.youtube.com/watch?v=hjQFrD1Vg7g) | 周杰倫 | Synced | YouTube Music |
| 38 | [天灰](https://music.youtube.com/watch?v=jlFPmLV1WEM) | S.H.E | Synced | LRCLib (exact) |
| 39 | [郭源潮](https://music.youtube.com/watch?v=M5sEXCJ3DGU) | 宋冬野 | Synced | YouTube Music |
| 40 | [陳韻若 - 愛的迴歸線『在愛的迴歸線 陽光在手指間』【動態歌詞Lyrics】](https://music.youtube.com/watch?v=_f2VVN19_Q8) | Music Channel HM | Synced | Kugeci |
| 41 | [Start From The Bottom](https://music.youtube.com/watch?v=gXh2AkfBoo8) | MC HotDog熱狗 | Synced | YouTube Music |
| 42 | [樓上的房東](https://music.youtube.com/watch?v=-KA4OU5SFxA) | MC HotDog熱狗 | Synced | YouTube Music |
| 43 | [Can You Hear The Music](https://music.youtube.com/watch?v=4JZ-o3iAJv4) | Ludwig Göransson | Instrumental | — |
| 44 | [讓我一個人過](https://music.youtube.com/watch?v=Yxc2w4isXuY) | Gordon Flanders | Synced | YouTube Music |
| 45 | [瞬](https://music.youtube.com/watch?v=eZJ6EiRoOWo) | 鄭潤澤 | Synced | YouTube Music |
| 46 | [聽爸爸的話](https://music.youtube.com/watch?v=-Aae5emEz38) | 周杰倫 | Synced | YouTube Music |
| 47 | [Luv (sic) pt6 Uyama Hiroto Remix Instrumentals](https://music.youtube.com/watch?v=2O-uZ8aoMsk) | Nujabes | Instrumental | — |
| 48 | [歌曲Top9《答案》杨坤 郭采洁 【2014年央视春晚】｜订阅CCTV春晚](https://music.youtube.com/watch?v=Q_3Oq2iRQ-s) | CCTV春晚 | Synced | Kugeci |
| 49 | [髮如雪](https://music.youtube.com/watch?v=Px_TOP_d9zY) | 周杰倫 | Synced | YouTube Music |
| 50 | [明明就](https://music.youtube.com/watch?v=yYa4minaocg) | 周杰倫 | Synced | YouTube Music |
