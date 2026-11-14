//! UI languages. Owner: ui slice.

use std::sync::LazyLock;

use ebobo_net::NetError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
}

impl Lang {
    /// `Config::lang` value ("ru"/"en") or the system language when `None`/unknown.
    pub fn resolve(config: Option<&str>) -> Lang {
        match config.map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s.starts_with("ru") => Lang::Ru,
            Some(s) if s.starts_with("en") => Lang::En,
            _ => system_lang(),
        }
    }

    /// Value stored in `Config::lang`.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "en",
        }
    }

    pub(crate) fn tr(self, s: S) -> &'static str {
        let (ru, en) = s.pair();
        match self {
            Lang::Ru => ru,
            Lang::En => en,
        }
    }

    /// Title, cause, and what to do for a network failure.
    pub(crate) fn net_error(self, err: &NetError) -> [&'static str; 3] {
        let ru = self == Lang::Ru;
        let pick = |r: [&'static str; 3], e: [&'static str; 3]| if ru { r } else { e };
        match err {
            NetError::Server(_) => pick(
                ["Сервер недоступен", "Не удалось связаться с сервером комнат.", "Проверь интернет и попробуй ещё раз. Можно играть по IP."],
                ["Server unreachable", "Could not reach the room server.", "Check your internet and try again, or play by IP."],
            ),
            NetError::RoomTaken => pick(
                ["Не удалось создать комнату", "Все выбранные коды заняты.", "Попробуй создать комнату ещё раз."],
                ["Could not create room", "Every code we tried was taken.", "Try creating the room again."],
            ),
            NetError::NoRoom => pick(
                ["Комната не найдена", "Комнаты с таким кодом нет.", "Проверь код у друга и введи его снова."],
                ["Room not found", "There is no room with this code.", "Check the code with your friend and retry."],
            ),
            NetError::RoomFull => pick(
                ["Комната заполнена", "В комнате уже 4 игрока.", "Попроси хоста создать новую комнату."],
                ["Room is full", "The room already has 4 players.", "Ask the host to make a new room."],
            ),
            NetError::RoomLocked => pick(
                ["Игра уже началась", "Хост уже запустил игру.", "Попроси хоста выйти в лобби и войди снова."],
                ["Game already started", "The host has already started.", "Ask the host to go back to the lobby."],
            ),
            NetError::HostLeft => pick(
                ["Хост вышел", "Хост закрыл комнату.", "Создай свою комнату или войди в другую."],
                ["Host left", "The host closed the room.", "Make your own room or join another."],
            ),
            NetError::Version => pick(
                ["Разные версии ebobo", "У друга другая версия ebobo. Проверяем обновление…", "Кто старее, обновится сам при запуске: перезапустите ebobo."],
                ["Different ebobo versions", "Your friend runs another version. Checking for an update…", "The older ebobo updates itself on start: restart it."],
            ),
            NetError::RomMissing => pick(
                ["Нет ROM", "У тебя нет этой игры, а передать её не вышло.", "Попроси хоста прислать ROM или положи его в папку ROMs."],
                ["ROM missing", "You lack this game and the transfer failed.", "Ask the host to send the ROM or put it in a ROM folder."],
            ),
            NetError::RomCrc => pick(
                ["ROM повреждён", "Полученный файл не совпал с ROM хоста.", "Войди в комнату ещё раз, чтобы получить ROM заново."],
                ["ROM corrupt", "The received file differs from the host's ROM.", "Join the room again to receive it anew."],
            ),
            NetError::BadRom(_) => pick(
                ["ROM не запускается", "Эмулятор не может загрузить эту игру.", "Выберите другой ROM."],
                ["ROM won't load", "The emulator cannot load this game.", "Pick another ROM."],
            ),
            NetError::Timeout => pick(
                ["Нет ответа", "По этому адресу никто не ответил.", "Проверь адрес и порт; хост должен ждать подключения."],
                ["No answer", "Nobody answered at this address.", "Check address and port; the host must be waiting."],
            ),
            NetError::Disconnected => pick(
                ["Связь потеряна", "Игрок вышел или перестал отвечать.", "Проверь интернет и начните заново."],
                ["Connection lost", "A player left or stopped answering.", "Check your internet and start again."],
            ),
            NetError::Io(_) => pick(
                ["Ошибка сети", "Не удалось открыть сетевое соединение.", "Проверь интернет и настройки брандмауэра."],
                ["Network error", "Could not open a network connection.", "Check your internet and firewall settings."],
            ),
        }
    }
}

static SYSTEM_LANG: LazyLock<Lang> = LazyLock::new(|| {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if v.is_empty() || v == "C" || v == "POSIX" {
                continue;
            }
            return if v.to_ascii_lowercase().starts_with("ru") { Lang::Ru } else { mac_lang().unwrap_or(Lang::En) };
        }
    }
    mac_lang().unwrap_or(Lang::En)
});

fn system_lang() -> Lang {
    *SYSTEM_LANG
}

/// macOS preferred languages (`defaults read -g AppleLanguages`): Russian when the first
/// listed language is Russian or any listed one is when no env var decides.
#[cfg(target_os = "macos")]
fn mac_lang() -> Option<Lang> {
    let out = std::process::Command::new("defaults").args(["read", "-g", "AppleLanguages"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    if text.contains("ru") {
        Some(Lang::Ru)
    } else {
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn mac_lang() -> Option<Lang> {
    None
}

/// Every fixed UI string; `Lang::tr` picks the language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum S {
    CreateRoom,
    JoinByCode,
    PlaySolo,
    ByIp,
    Settings,
    Quit,
    PickRomSolo,
    PickRomHost,
    PickRomDirect,
    Found,
    Rescan,
    NoRoms,
    PutRomsIn,
    RomsHome,
    RomsDeck,
    RomsDownloads,
    Scanning,
    Unsupported,
    EnterCode,
    CodeHint,
    Join,
    WaitForPlayers,
    ConnectTo,
    DirectHint,
    AddrTitle,
    NickTitle,
    ServerTitle,
    Nick,
    Fullscreen,
    Scale,
    ScreenMode,
    ScreenStretch,
    ScreenFit,
    ScreenPixel,
    Effect,
    EffectOff,
    EffectCrtTv,
    EffectPvm,
    EffectLcd,
    Volume,
    InputDelay,
    ForceRelay,
    SpriteLimit,
    SpriteLimitNes,
    SpriteLimitRemove,
    Language,
    Server,
    Gamepads,
    NoPads,
    Auto,
    On,
    Off,
    Room,
    TellCode,
    Waiting,
    You,
    Start,
    NeedPlayer,
    NeedRom,
    NeedReady,
    WaitHost,
    Connecting,
    Starting,
    LeaveRoom,
    Leave,
    Stay,
    Paused,
    NetRunning,
    Resume,
    SaveState,
    LoadState,
    Reset,
    ResetAll,
    ExitToMenu,
    ExitGame,
    Menu,
    Select,
    Back,
    Change,
    Page,
    Ok,
    Error,
    PadConnected,
    PadDisconnected,
    PlayerN,
    PeerInterrupted,
    PeerLeft,
    Desync,
    StateSaved,
    StateLoaded,
    NoQuickSave,
    Slot,
    SlotEmpty,
    StateWrongRom,
    StateOldVersion,
    StateCorrupt,
    StateSaveFailed,
    MinAgo,
    HoursAgo,
    DaysAgo,
    LeaveGameQ,
    Advanced,
    Edit,
    NoRomOnline,
    NetCheck,
    Recheck,
    CheckServer,
    CheckRooms,
    CheckPublic,
    CheckLocal,
    CheckNat,
    NatCone,
    NatSymmetric,
    NatBlocked,
    Unreachable,
    Works,
    NotWorking,
    Checking,
    WhatToDo,
    AdviceServer,
    AdviceRooms,
    AdviceSymmetric,
    AdviceBlocked,
    AdviceAllGood,
    Reconnecting,
    SleepGap,
    ScreenshotSaved,
    ScreenshotFailed,
    UpdatingTo,
    UpdateMoveToApps,
    UpdateRerunInstaller,
    AutoUpdate,
    FastForward,
    FfHold,
    FfToggle,
    HintRewind,
    HintFastForward,
    HintScreenshot,
}

impl S {
    fn pair(self) -> (&'static str, &'static str) {
        use S::*;
        match self {
            CreateRoom => ("Создать комнату", "Create room"),
            JoinByCode => ("Войти по коду", "Join by code"),
            PlaySolo => ("Играть одному", "Play solo"),
            ByIp => ("Подключение по IP", "Connect by IP"),
            Settings => ("Настройки", "Settings"),
            Quit => ("Выход", "Quit"),
            PickRomSolo => ("Выбери игру", "Pick a game"),
            PickRomHost => ("Игра для комнаты", "Game for the room"),
            PickRomDirect => ("Игра для хоста", "Game to host"),
            Found => ("Найдено", "Found"),
            Rescan => ("Обновить список", "Rescan"),
            NoRoms => ("Игры не найдены", "No games found"),
            PutRomsIn => ("Положи .nes или .zip в:", "Put .nes or .zip files in:"),
            RomsHome => ("~/ROMs", "~/ROMs"),
            RomsDeck => ("Emulation/roms/nes", "Emulation/roms/nes"),
            RomsDownloads => ("~/Downloads", "~/Downloads"),
            Scanning => ("Ищем игры", "Scanning"),
            Unsupported => ("mapper", "mapper"),
            EnterCode => ("Код комнаты", "Room code"),
            CodeHint => ("Введи код от друга", "Type your friend's code"),
            Join => ("Войти", "Join"),
            WaitForPlayers => ("Ждать игроков", "Wait for players"),
            ConnectTo => ("Подключиться", "Connect"),
            DirectHint => ("Хост должен открыть порт", "Host must open the port"),
            AddrTitle => ("Адрес хоста:порт", "Host address:port"),
            NickTitle => ("Твой ник", "Your nickname"),
            ServerTitle => ("Адрес сервера", "Server address"),
            Nick => ("Ник", "Nickname"),
            Fullscreen => ("Полный экран", "Fullscreen"),
            Scale => ("Масштаб окна", "Window scale"),
            ScreenMode => ("Экран", "Screen"),
            ScreenStretch => ("Растянуть", "Stretch"),
            ScreenFit => ("Пропорции", "Fit"),
            ScreenPixel => ("Пиксели", "Pixel"),
            Effect => ("Эффект", "Effect"),
            EffectOff => ("Выкл", "Off"),
            EffectCrtTv => ("ЭЛТ телевизор", "CRT TV"),
            EffectPvm => ("Монитор", "Monitor"),
            EffectLcd => ("LCD", "LCD"),
            Volume => ("Громкость", "Volume"),
            InputDelay => ("Задержка ввода", "Input delay"),
            ForceRelay => ("Через ретранслятор", "Always relay"),
            SpriteLimit => ("Лимит спрайтов", "Sprite limit"),
            SpriteLimitNes => ("Как на NES", "Like NES"),
            SpriteLimitRemove => ("Убрать", "Remove"),
            Language => ("Язык", "Language"),
            Server => ("Сервер", "Server"),
            Gamepads => ("Геймпады", "Gamepads"),
            NoPads => ("Не подключены", "None connected"),
            Auto => ("Авто", "Auto"),
            On => ("Да", "On"),
            Off => ("Нет", "Off"),
            Room => ("Комната", "Room"),
            TellCode => ("Скажи код друзьям", "Tell friends the code"),
            Waiting => ("ждём...", "waiting..."),
            You => ("ты", "you"),
            Start => ("Начать игру", "Start game"),
            NeedPlayer => ("Нужен ещё хотя бы 1 игрок", "Need at least 1 more player"),
            NeedRom => ("Ждём передачу ROM", "Waiting for ROM transfer"),
            NeedReady => ("Ждём готовности игроков", "Waiting for players"),
            WaitHost => ("Ждём, пока хост начнёт", "Waiting for the host"),
            Connecting => ("Подключаемся", "Connecting"),
            Starting => ("Запускаем", "Starting"),
            LeaveRoom => ("Выйти из комнаты?", "Leave the room?"),
            Leave => ("Выйти", "Leave"),
            Stay => ("Остаться", "Stay"),
            Paused => ("Пауза", "Paused"),
            NetRunning => ("Игра у всех идёт дальше", "The game keeps running"),
            Resume => ("Продолжить", "Resume"),
            SaveState => ("Сохранить (F5)", "Save state (F5)"),
            LoadState => ("Загрузить (F7)", "Load state (F7)"),
            Reset => ("Сброс", "Reset"),
            ResetAll => ("Сброс для всех", "Reset for everyone"),
            ExitToMenu => ("Выйти в меню", "Exit to menu"),
            ExitGame => ("Выйти из игры", "Leave game"),
            Menu => ("Меню", "Menu"),
            Select => ("Выбрать", "Select"),
            Back => ("Назад", "Back"),
            Change => ("Изменить", "Change"),
            Page => ("Стр.", "Page"),
            Ok => ("OK", "OK"),
            Error => ("Ошибка", "Error"),
            PadConnected => ("Геймпад", "Gamepad"),
            PadDisconnected => ("отключён", "disconnected"),
            PlayerN => ("игрок", "player"),
            PeerInterrupted => ("Ждём игрока {name}... {secs} с", "Waiting for {name}... {secs} s"),
            PeerLeft => ("{name} вышел из игры", "{name} left the game"),
            Desync => ("Рассинхрон! Игры разошлись", "Desync! Games diverged"),
            StateSaved => ("Сохранено", "State saved"),
            StateLoaded => ("Загружено", "State loaded"),
            NoQuickSave => ("Нет сохранения", "No saved state"),
            Slot => ("Слот", "Slot"),
            SlotEmpty => ("пусто", "empty"),
            StateWrongRom => ("Сохранение от другой игры", "Save is from another game"),
            StateOldVersion => ("Сохранение от старой версии ebobo", "Save is from an old ebobo version"),
            StateCorrupt => ("Файл сохранения повреждён", "Save file is corrupt"),
            StateSaveFailed => ("Не удалось сохранить", "Could not save"),
            MinAgo => ("{n} мин назад", "{n} min ago"),
            HoursAgo => ("{n} ч назад", "{n} h ago"),
            DaysAgo => ("{n} дн назад", "{n} d ago"),
            LeaveGameQ => ("Выйти из игры?", "Leave the game?"),
            Advanced => ("Дополнительно", "Advanced"),
            Edit => ("Изменить", "Edit"),
            NoRomOnline => ("Нельзя открыть ROM во время онлайн-игры", "Can't open a ROM during an online game"),
            NetCheck => ("Проверка сети", "Network check"),
            Recheck => ("Проверить снова", "Check again"),
            CheckServer => ("Сервер", "Server"),
            CheckRooms => ("Комнаты", "Rooms"),
            CheckPublic => ("Адрес", "Address"),
            CheckLocal => ("Локальный IP", "Local IP"),
            CheckNat => ("NAT", "NAT"),
            NatCone => ("обычный", "normal"),
            NatSymmetric => ("симметричный", "symmetric"),
            NatBlocked => ("UDP заблокирован", "UDP blocked"),
            Unreachable => ("недоступен", "unreachable"),
            Works => ("работают", "working"),
            NotWorking => ("не работают", "not working"),
            Checking => ("Проверяем, до 6 секунд", "Checking, up to 6 seconds"),
            WhatToDo => ("Что делать:", "What to do:"),
            AdviceServer => (
                "Сервер недоступен. Проверь интернет. Если не помогает (бывает в России), играйте по IP: \"Подключение по IP\" через Tailscale/ZeroTier или открытый UDP-порт 7777.",
                "Server unreachable. Check your internet. If that is not it, play by IP: \"Connect by IP\" over Tailscale/ZeroTier or a forwarded UDP port 7777.",
            ),
            AdviceRooms => (
                "Сервер отвечает, но комнаты не открываются: сеть режет WebSocket. Попробуй другую сеть или играйте по IP через Tailscale/ZeroTier.",
                "The server answers but rooms do not open: the network blocks WebSocket. Try another network or play by IP over Tailscale/ZeroTier.",
            ),
            AdviceSymmetric => (
                "NAT симметричный (мобильный интернет или CGNAT): напрямую не пробиться, ebobo сам играет через ретранслятор. Пинг будет чуть выше.",
                "Symmetric NAT (mobile internet or CGNAT): no direct link, ebobo plays through the relay by itself. Ping will be a bit higher.",
            ),
            AdviceBlocked => (
                "UDP заблокирован (корпоративная или гостевая сеть): работает только ретранслятор.",
                "UDP is blocked (office or guest network): only the relay works.",
            ),
            AdviceAllGood => (
                "Всё в порядке: комнаты и прямое соединение (P2P) должны работать.",
                "All good: rooms and direct (P2P) connections should work.",
            ),
            Reconnecting => ("переподключается...", "reconnecting..."),
            SleepGap => ("Связь прерывалась (сон)", "Connection paused (sleep)"),
            ScreenshotSaved => ("Снимок: {name}", "Screenshot: {name}"),
            ScreenshotFailed => ("Не удалось сохранить снимок", "Could not save the screenshot"),
            UpdatingTo => ("Обновляемся до {name}…", "Updating to {name}…"),
            UpdateMoveToApps => (
                "Для обновлений перенеси ebobo в Программы (github.com/idoo/ebobo)",
                "To update, move ebobo to Applications (github.com/idoo/ebobo)",
            ),
            UpdateRerunInstaller => (
                "Есть новая версия: запусти установщик ebobo ещё раз",
                "New version out: run the ebobo installer again",
            ),
            AutoUpdate => ("Автообновление", "Auto-update"),
            FastForward => ("Ускорение", "Fast-forward"),
            FfHold => ("пока держишь", "hold"),
            FfToggle => ("вкл/выкл", "toggle"),
            HintRewind => ("Назад во времени: {keys}", "Rewind: {keys}"),
            HintFastForward => ("Ускорение: {keys}", "Fast-forward: {keys}"),
            HintScreenshot => ("Снимок экрана: {keys}", "Screenshot: {keys}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_explicit() {
        assert_eq!(Lang::resolve(Some("ru")), Lang::Ru);
        assert_eq!(Lang::resolve(Some("en")), Lang::En);
    }
}
