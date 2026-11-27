# Mi Clip

Mi Clip 是一款 macOS 菜单栏工具，可以找回复制过的内容、截图并标注，也能通过局域网和小米手机同步短文本、互传文件。手机端使用系统自带的小米互联。

<p>
    <img src="assets/macos-panel.png" alt="Mac 菜单栏面板和已发现的设备" width="280">
    <img src="assets/phone-share.png" alt="小米手机系统分享页中的 Clip" width="280">
</p>

## 安装与使用

在 [GitHub Releases](https://github.com/mnhkahn/mi-clip/releases/latest) 下载对应芯片的应用包。Apple Silicon 选 `Clip-macos-aarch64.zip`，Intel 选 `Clip-macos-x86_64.zip`。解压后将 `Clip.app` 拖入“应用程序”并打开。

- **剪贴板历史**　按 `⌥·`（Option 加数字 1 左边的键）搜索记录。选中后按回车或双击可粘贴到原应用，需要辅助功能权限。按 `⌘↵` 可仅复制。历史保存在本机，最多 500 条、共 512 MiB。
- **截图与标注**　按 `⌃⌘A` 框选区域，可加文字、打码或滚动截长图，再复制或保存为 PNG。首次使用需允许屏幕录制。

## 小米互联

让 Mac 和手机连入同一局域网，在手机上开启小米互联。首次使用时，在 Mac 上用与手机相同的小米账号登录，然后在 Clip 菜单栏面板开启“局域网设备”。

<details>
<summary>首次登录命令（已安装 Clip.app）</summary>

在终端运行以下命令，用手机扫描打开的二维码并确认登录。

```sh
umask 077
clip_bin="/Applications/Clip.app/Contents/MacOS/mi-clip"
clip_dir="$HOME/Library/Application Support/MiClip"
mkdir -p "$clip_dir"
chmod 700 "$clip_dir"
"$clip_bin" --auth prepare "$clip_dir/qr-state.json" "$clip_dir/qr.png"
open "$clip_dir/qr.png"
```

扫码确认后，在同一终端依次运行以下命令。配置文件和证书仅供本机使用。

```sh
"$clip_bin" --auth poll "$clip_dir/qr-state.json" "$clip_dir/grant.json"
"$clip_bin" --auth redeem "$clip_dir/grant.json" "$clip_dir/session.json"
"$clip_bin" --auth account-cert "$clip_dir/session.json" "$clip_dir/account.pem"
"$clip_bin" --auth device-cert "$clip_dir/session.json" "$clip_dir/device"
cat > "$clip_dir/config.json" <<'JSON'
{
  "session": "session.json",
  "identity": "device/device.json",
  "account_certificate": "account.pem"
}
JSON
chmod 600 "$clip_dir/config.json"
```

退出并重新打开 Clip，解锁手机以完成首次发现。

</details>

- **短文本**　在一端复制，在另一端粘贴。单次最多 3072 字节（UTF-8）。
- **Mac 发到手机**　在 Finder 选中文件，右键选择“共享 → Clip 分享”，再选择手机。也可以先在 Mac 复制图片或普通文件，再到手机上的兼容应用中粘贴。
- **手机发到 Mac**　在手机系统分享页选择“Clip”，接收的文件会保存在 Mac 的 `~/Downloads`。

双向文本和文件分享已在 Intel Mac 与一台 HyperOS 3 手机上验证，手机端图片粘贴已在微信验证。

正式版会统计匿名使用事件，剪贴板正文和文件名不在统计范围内。设置环境变量 `MICLIP_NO_ANALYTICS=1` 或 `DO_NOT_TRACK=1` 可关闭统计。

### 互联协议怎样传数据

同一张图片，在微信里粘贴和从 Finder 右键分享，正文会走两条不同的通道。[详解：KCP、手机发现与四类传输通道](docs/transfer-protocol.md)

一、**发现与认证**　Clip 通过 mDNS 找到同一局域网里的手机。双方验证设备证书，确认使用同一小米账号，并协商本次连接的密钥。

二、**打包**　短文本直接以 UTF-8 编码放进剪贴板消息。在 Mac 复制图片或文件时，Clip 先发送文件名和类型，等手机请求内容后再分块读取。

三、**加密与发送**　互联传输没有额外的压缩步骤。Clip 用 AES-GCM 加密消息或文件块，并附上认证标签，供接收端检查数据是否被改动。

四、**解密与接收**　接收端在解密时验证认证标签。Mac 将收到的文本写入剪贴板。接收文件时，Mac 逐块核对位置，收齐并确认总大小后才将文件放入下载目录。

## 从源码构建

在装有 Xcode Command Line Tools 和 Rust 1.95 或更高版本的 Mac 上运行 `sh tools/build-app.sh`。生成的 `target/app/Clip.app` 包含 Finder 共享扩展。

## MIT 开源协议

MIT 允许任何人使用、复制、修改、分享或销售 Mi Clip。例如，你可以把 Clip 的截图功能集成到收费的闭源应用里，自己修改的代码也可以保持闭源。

**需要保留的声明**　复制整个软件或其中的实质性部分时，副本中须保留原版权声明和许可声明。发布上述应用时，可在随应用分发的“开源许可”中收录本项目的 [LICENSE 全文](LICENSE)。

**担保与责任**　软件按现状提供。作者和版权持有人不保证它适合某项用途，也不对因使用软件引起的索赔或损失承担责任。
