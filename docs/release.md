# リリース手順

配布する exe は Certum のオープンソース開発者向けコード署名証明書 (SimplySign のクラウド鍵) で
ローカル署名する。署名鍵は GitHub Actions からは使えないため、CI は検査 (fmt / clippy / test) だけを行い、
Releases への添付は手作業で行う。

## 事前準備 (初回のみ)

- Windows SDK (signtool.exe / rc.exe)
- `cargo install --locked cargo-about --features cli` (THIRD-PARTY-NOTICES.txt の生成に使う)
- GitHub CLI (`gh`)

## 手順

1. `Cargo.toml` の `version` を上げ、変更内容をコミットする。
2. SimplySign Desktop を起動してログインする (トレイ通知で「SimplySign connected」を確認)。
3. 配布物を作る。

   ```powershell
   scripts\build-dist.ps1
   ```

   fmt / clippy / test → アプリ部分を clean して release ビルド → 署名・検証 →
   THIRD-PARTY-NOTICES.txt 生成 → zip と SHA-256 の作成、まで行う。
   成果物は `dist\aipicdl-v<version>-windows-x64.zip` と `.sha256`。
4. 署名を確認する (発行元が `Open Source Developer Taku Sano` で、タイムスタンプ付きであること)。

   ```powershell
   signtool verify /pa /v target\release\aipicdl.exe
   ```

5. タグを打って push し、Release を作る。

   ```powershell
   git tag v<version>
   git push origin main v<version>
   gh release create v<version> dist\aipicdl-v<version>-windows-x64.zip dist\aipicdl-v<version>-windows-x64.zip.sha256 --title v<version> --notes-file <notes.md>
   ```

## 注意

- SimplySign のセッションが切れていると、証明書ストアに証明書が見えていても `signtool sign` が
  「No certificates were found」で失敗することがある。その場合は SimplySign Desktop にログインし直す。
- `-NoSign` で作った zip は動作確認用。公開しないこと。
- リクエスト間隔 (API 3 秒 / ファイル 1 秒) や直列実行を緩める変更はリリースに含めない
  (README の「サイトへの配慮について」を参照)。
