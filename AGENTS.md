# AGENTS.md — Autonomous Execution Constitution

## 1. Zero-Human-In-The-Loop Policy (Quy Tắc Tuyệt Đối Không Hỏi Người Dùng)
- BẠN ĐANG CHẠY Ở CHẾ ĐỘ TỰ HÀNH 100% (FULLY AUTONOMOUS MODE).
- TUYỆT ĐỐI KHÔNG dừng lại để hỏi người dùng về lựa chọn thư viện, thiết kế giao diện, tên biến, cách xử lý ngoại lệ hay xin phép chạy lệnh kiểm thử.
- Mọi quyết định kiến trúc đã được khóa chặt trong tài liệu này. Nếu gặp bất kỳ điểm chưa rõ ràng nào trong quá trình triển khai chi tiết, hãy tự chọn giải pháp đơn giản nhất, chuẩn mực nhất trong hệ sinh thái Rust/Kotlin/Swift, ghi chú ngắn gọn vào docstring và tiếp tục thực thi.
- Không bao giờ để lại mã giả (`todo!()`, `unimplemented!()`, `// TODO`) khi kết thúc một Milestone.

## 2. Locked Technology Stack (Ngăn Xếp Công Nghệ Cố Định)
Không thay đổi hoặc thay thế các thư viện cốt lõi sau:
- **Ngôn ngữ Lõi & Desktop:** Rust (Stable toolchain, Edition 2021/2024).
- **Text Rope & Buffer:** `ropey = "1"`
- **Incremental Syntax Parsing:** `tree-sitter = "0.24"` (tích hợp sẵn grammar cho Rust, TypeScript/JS, Python, JSON, Markdown, Kotlin, Swift).
- **Text Shaping & Layout:** `cosmic-text = "0.12"`
- **Async Runtime & Concurrency:** `tokio = { version = "1", features = ["full"] }`, `parking_lot = "0.12"`
- **LSP Protocol Types:** `lsp-types = "0.97"`, `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`
- **Git Engine:** `gix = { version = "0.66", default-features = false, features = ["max-performance-safe"] }`
- **Terminal Emulation (Desktop):** `alacritty_terminal = "0.24"`
- **Cross-Language FFI Generator:** `uniffi = "0.28"`
- **Desktop GUI (macOS, Windows, Linux):** `iced = { version = "0.13", features = ["tokio", "canvas", "advanced"] }`
- **Android Native UI:** Kotlin 2.0+, Jetpack Compose (Material 3), Android MinSDK 26.
- **iOS Native UI:** Swift 6.0+, SwiftUI, iOS Deployment Target 17.0+.

## 3. Core Data Contract (`ide-core` & `ide-ffi`)
Tất cả các nền tảng (Desktop, Android, iOS) giao tiếp với nhân Rust thông qua mô hình Unidirectional Data Flow với các cấu trúc dữ liệu bắt buộc sau:

1. `EditorAction` (Enum):
   - `OpenFile { path: String }`
   - `SaveFile { buffer_id: u64 }`
   - `InsertText { buffer_id: u64, text: String }`
   - `DeleteBackward { buffer_id: u64 }`
   - `DeleteForward { buffer_id: u64 }`
   - `MoveCursor { buffer_id: u64, direction: CursorDirection, select: bool }`
   - `SetCursorPosition { buffer_id: u64, line: u32, column: u32, select: bool }`
   - `ScrollViewport { buffer_id: u64, first_visible_line: u32, visible_line_count: u32 }`
   - `Undo { buffer_id: u64 }`, `Redo { buffer_id: u64 }`
   - `TriggerLspCompletion { buffer_id: u64 }`
   - `SubmitAgentPrompt { prompt: String }`

2. `ViewportViewModel` (Record xuất qua UniFFI và dùng trên Desktop):
   - `buffer_id: u64`
   - `file_path: String`
   - `is_dirty: bool`
   - `total_lines: u32`
   - `first_visible_line: u32`
   - `visible_lines: Vec<RenderedLine>` (chỉ chứa các dòng nằm trong khung nhìn hiện tại để đảm bảo 120fps)
   - `cursors: Vec<CursorRect>`
   - `selections: Vec<SelectionRange>`
   - `diagnostics: Vec<DiagnosticItem>`
   - `git_diff_markers: Vec<LineDiffStatus>`
   - `agent_tasks: Vec<AgentTaskStatus>`

3. `RenderedLine`:
   - `line_number: u32`
   - `text: String`
   - `spans: Vec<HighlightSpan>` (`start_col: u32`, `end_col: u32`, `color_rgba: u32`, `font_style: u8`)

## 4. Sequential Execution Milestones & Verification Gates
Thực thi lần lượt từng Milestone. Ở cuối mỗi Milestone, BẮT BUỘC chạy lệnh kiểm chứng tương ứng và sửa hết mọi lỗi trước khi chuyển sang Milestone kế tiếp:

### Milestone 1: Khởi tạo Cargo Workspace & `ide-buffer` + `ide-syntax`
- Thiết lập Cargo Workspace với đầy đủ các crate trong `crates/`.
- Triển khai `TextBuffer` dựa trên `ropey::Rope` hỗ trợ UTF-8/UTF-16 offset conversion, multi-cursor, lịch sử Undo/Redo theo nhóm giao dịch (Transaction grouping), và tìm kiếm chuỗi nhanh.
- Triển khai bộ phân tích cú pháp gia tăng bằng `tree-sitter`, tự động cập nhật `InputEdit` mỗi khi `TextBuffer` thay đổi và trích xuất `Vec<HighlightSpan>` cho vùng dòng được yêu cầu.
- **Verification Gate 1:** `cargo test -p ide-buffer -p ide-syntax && cargo clippy --workspace -- -D warnings` (Viết tối thiểu 15 unit test kiểm tra chèn/xóa Unicode tiếng Việt, emoji, undo/redo, và incremental parsing).

### Milestone 2: Triển khai `ide-lsp`, `ide-git` và `ide-agent`
- Xây dựng client JSON-RPC 2.0 bất đồng bộ trong `ide-lsp` hỗ trợ hai chế độ: `StdioTransport` (cho Desktop) và `WebSocketTransport` (cho Mobile kết nối tới Remote LSP server). Hỗ trợ: `textDocument/didOpen`, `didChange`, `completion`, `hover`, `definition`, `publishDiagnostics`.
- Xây dựng `ide-git` dùng `gix` để đọc nhánh hiện tại, danh sách tệp thay đổi và tính toán trạng thái thêm/sửa/xóa trên từng dòng (gutter diff markers).
- Xây dựng `ide-agent` lấy cảm hứng từ Google Antigravity: quản lý danh sách `AgentTask` chạy nền bất đồng bộ, có khả năng đọc/ghi `TextBuffer`, tạo bản vá (Unified Diff patch), áp dụng thay đổi vào bộ nhớ đệm và theo dõi trạng thái xác minh.
- **Verification Gate 2:** `cargo test -p ide-lsp -p ide-git -p ide-agent && cargo clippy --workspace -- -D warnings` (Dùng mock transport để test toàn bộ luồng LSP và Agent patch).

### Milestone 3: Hợp nhất `ide-core` & Cầu nối `ide-ffi` (Mozilla UniFFI)
- Kết nối toàn bộ các mô-đun vào `IdeWorkspace` trong `ide-core`.
- Thiết lập `ide-ffi` với `crate-type = ["cdylib", "staticlib", "lib"]` và viết trình tạo mã `uniffi-bindgen` tự động xuất file `.kt` (Kotlin) và `.swift` (Swift).
- **Verification Gate 3:** `cargo test -p ide-core -p ide-ffi` và chạy lệnh sinh thử nghiệm bindings cho Kotlin và Swift vào thư mục tạm để xác nhận không có lỗi ánh xạ kiểu dữ liệu.

### Milestone 4: Xây dựng Ứng dụng Desktop (`ide-desktop` cho macOS, Windows, Linux)
- Sử dụng `iced` và `cosmic-text` để xây dựng giao diện hai chế độ (Dual-View như Antigravity):
  1. **Editor View:** Cây thư mục bên trái, vùng soạn thảo mã nguồn ở giữa (vẽ số dòng, git gutter, syntax spans, con trỏ, minimap cơ bản), bảng Terminal/Diagnostics ở dưới, và Command Palette (`Cmd/Ctrl + P`, `Cmd/Ctrl + Shift + P`).
  2. **Agent Manager View:** Giao diện điều phối các tác vụ AI Agent chạy song song, hiển thị Diff Preview trực quan (Chấp nhận / Từ chối thay đổi).
- **Verification Gate 4:** `cargo build --release -p ide-desktop && cargo test -p ide-desktop` (Đảm bảo biên dịch thành công không có cảnh báo).

### Milestone 5: Xây dựng Ứng dụng Native Android (`platforms/android` - Kotlin)
- Tạo cấu trúc dự án Gradle chuẩn sử dụng Kotlin & Jetpack Compose.
- Tích hợp thư viện native `libide_ffi.so` và tệp bindings Kotlin do UniFFI sinh ra.
- Xây dựng `CodeEditorComposable` dùng `Canvas` + `TextMeasurer` để hiển thị `ViewportViewModel.visible_lines`, thanh công cụ phím tắt lập trình (Tab, `{`, `}`, `(`, `)`, `;`, Esc, Undo/Redo) phía trên bàn phím ảo, ngăn kéo cây thư mục (ModalNavigationDrawer) và màn hình quản lý Agent.
- **Verification Gate 5:** Kiểm tra cú pháp Kotlin, cấu hình Gradle và kịch bản build cross-compile (`cargo-ndk`).

### Milestone 6: Xây dựng Ứng dụng Native iOS (`platforms/ios` - Swift)
- Tạo cấu trúc Swift Package / Xcode project sử dụng Swift 6 & SwiftUI.
- Tích hợp bindings Swift từ UniFFI và script đóng gói `XCFramework`.
- Xây dựng `CodeEditorView` tối ưu cho cả iPhone (NavigationStack + InputAccessoryView chứa phím lập trình) và iPad (NavigationSplitView + hỗ trợ phím tắt bàn phím cứng `UIKeyCommand`).
- Tạo quy trình CI/CD trên GitHub Actions (`.github/workflows/ci.yml`) tự động biên dịch và đóng gói bản phát hành cho cả 5 hệ điều hành (macOS `.dmg`, Windows `.exe`, Linux AppImage/tar.gz, Android `.apk`, iOS `.xcframework`).
- **Verification Gate 6:** Chạy toàn bộ `cargo test --workspace`, kiểm tra tính đầy đủ của các tệp nguồn trên cả 5 nền tảng và hoàn tất tài liệu `README.md`.
