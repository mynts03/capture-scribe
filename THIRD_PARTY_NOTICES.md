# 第三者の権利表示

## Tauriアプリケーションテンプレート

`src-tauri/build.rs`、`src-tauri/src/main.rs` の起動用定型部分、`src/vite-env.d.ts` は、[create-tauri-app](https://github.com/tauri-apps/create-tauri-app) のテンプレートと同じ、またはそれを変更した記述を含みます。

Copyright (c) 2019 - 2022 Tauri Programme within The Commons Conservancy

これらのテンプレート由来の部分には [MITライセンス](LICENSES/create-tauri-app-MIT.txt) が適用されます。新しいCaptureScribeのアイコンはTauriのテンプレートアイコンを含みません。

## ビルド時に取得する依存関係

npmとCargoの依存関係の版と出所は `package-lock.json` と `src-tauri/Cargo.lock` に記載しています。取得される各コンポーネントには供給元の著作権表示と利用条件が適用されます。このソース一式にはそれらの依存パッケージや完成アプリは同梱しません。

## 利用者が別途用意するコンポーネント

[FFmpeg・ffprobe](https://ffmpeg.org/legal.html) と [Python](https://docs.python.org/3/license.html) は別途用意します。FFmpegの適用条件は入手したビルドの構成によって異なります。macOSのフレームワーク、Apple SDK、Apple Speechの言語モデルはAppleから提供されます。
