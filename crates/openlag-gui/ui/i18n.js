// Interface language. The UI is written in Russian; for English a MutationObserver
// translates every text node and the placeholder/title/aria-label attributes as they
// appear, by exact phrase or by pattern (for phrases with numbers and names inside).
// Strings drawn on canvases or in xterm go through I18N.t() explicitly.
"use strict";

const I18N = (() => {
  const norm = (s) => s.replace(/\s+/g, " ").trim();

  const EN_RAW = {
    // window and navigation
    "Свернуть": "Minimize", "Развернуть": "Maximize", "Закрыть": "Close",
    "Главная": "Home", "Серверы": "Servers", "Подписки": "Subscriptions", "Процессы": "Processes",
    "Терминал": "Terminal", "Настройки": "Settings", "Фан": "Fun",
    // home
    "Включить оптимизацию": "Turn on optimization", "Выключено": "Off", "Оптимизация включена": "Optimization on",
    "Оптимизация выключена": "Optimization off",
    "Нажмите, чтобы пустить игры через выбранный сервер": "Click to route your games through the selected server",
    "Сервер": "Server", "пинг до сервера, мс": "ping to server, ms", "Джиттер": "Jitter", "Потери": "Loss",
    "Пинг до тех же серверов игры: напрямую с вашей сетевой карты и через OpenLag (по ответам самой игры)":
      "Ping to the same game servers: directly from your network card and through OpenLag (from the game's own replies)",
    "Без OpenLag → с OpenLag": "Without OpenLag → with OpenLag",
    "появится, когда игра начнёт пинговать свои серверы": "appears once the game starts pinging its servers",
    "ждём, пока игра пропингует свои серверы": "waiting for the game to ping its servers",
    "Соединений": "Connections", "Пинг, мс": "Ping, ms", "Потери пакетов, %": "Packet loss, %", "Соединения": "Connections",
    "Тип": "Type", "Куда": "Destination", "Через": "Via",
    "ICMP напрямую с сетевой карты, в обход любых туннелей": "ICMP straight from the network card, bypassing any tunnels",
    "Без OpenLag": "Without OpenLag", "С OpenLag": "With OpenLag", "Трафик": "Traffic",
    "Время ответа сервера на пакеты самой игры через туннель": "Server reply time to the game's own packets through the tunnel",
    "Пока нет. Включите оптимизацию и запустите программу.": "Nothing yet. Turn on optimization and start a program.",
    "нет ответа": "no reply", "напрямую": "direct", "поток": "stream",
    "Поток матча: игра шлёт пакеты потоком, пинг видно в самой игре": "Match stream: the game sends a steady flow, see the ping in the game itself",
    "нет доступных серверов": "no servers available", "процессы не выбраны": "no processes selected",
    "Без прав администратора перехват трафика не работает. Пинг до серверов меряется и так.":
      "Traffic capture needs administrator rights. Server ping is measured anyway.",
    "Перезапустить от администратора": "Restart as administrator",
    "Нет WinDivert.dll / WinDivert64.sys рядом с OpenLag: перехват недоступен.": "WinDivert.dll / WinDivert64.sys are missing next to OpenLag: capture is unavailable.",
    "Нет серверов. Добавьте подписку от VPN-сервиса.": "No servers. Add a subscription from your VPN service.",
    "Добавить подписку": "Add subscription", "Не выбраны процессы.": "No processes selected.", "Выбрать процессы": "Select processes",
    "Обновить": "Update",
    // servers
    "Поиск": "Search", "Авто": "Auto",
    "Пинг меряется по UDP через сам сервер, так же как пойдут пакеты игры. Новые соединения идут через выбранный сервер, а в режиме «Авто» через сервер с лучшим пингом. Отключённые переключателем серверы не запускаются и никогда не выбираются.":
      "Ping is measured over UDP through the server itself, the way game packets will go. New connections use the selected server, or in “Auto” the one with the best ping. Servers switched off are not started and never picked.",
    "Страны выхода": "Exit countries", "По пингу": "By ping", "По странам": "By country", "Перепроверить страны": "Re-check countries",
    "Страна определяется не по названию, а по факту: через каждый сервер запрашивается Cloudflare, и он сообщает, с какого IP и из какой страны реально выходит трафик. Щелчок по стране отключает или включает все её серверы.":
      "The country comes from the facts, not the name: OpenLag asks Cloudflare through each server which IP and country the traffic really exits from. Click a country to switch all its servers off or on.",
    "Фильтры: отключить серверы по слову": "Filters: switch servers off by word", "например: Russia": "e.g. Russia",
    "Добавить": "Add", "Серверов нет. Добавьте подписку на вкладке «Подписки».": "No servers. Add a subscription on the “Subscriptions” tab.",
    "пинг": "ping", "джиттер": "jitter", "потери": "loss", "ошибка": "error", "Выбрать": "Select", "Выбран": "Selected",
    "свой relay": "own relay", "Отключённые": "Switched off", "Страна ещё не определена": "Country not detected yet",
    "Отключён фильтром: уберите слово в фильтрах выше": "Switched off by a filter: remove the word from the filters above",
    "Включить / отключить сервер": "Turn server on / off", "Авто-серверы": "Auto servers",
    "Страны появятся через несколько секунд после запуска серверов.": "Countries appear a few seconds after the servers start.",
    "Авто-серверы отключены": "Auto servers switched off", "Авто-серверы включены": "Auto servers switched on",
    "Перепроверяю страны выхода…": "Re-checking exit countries…", "Убрать фильтр": "Remove filter",
    "Нет фильтров. Серверы со словом из списка в названии или адресе отключаются целиком и не выбираются в «Авто», даже после обновления подписки.":
      "No filters. Servers with a listed word in their name or address are switched off entirely and never picked by “Auto”, even after the subscription updates.",
    "Автовыбор: сервер с лучшим пингом": "Auto: the server with the best ping", "Автовыбор сервера": "Automatic server choice",
    "Включить все авто-серверы": "Turn on all auto servers", "Отключить все авто-серверы": "Turn off all auto servers",
    // subscriptions
    "Ссылка на подписку или ключи": "Subscription link or keys",
    "https://… (Happ, Incy, Marzban, Remnawave) vless://… vmess://… trojan://… ss://… vpn://… (Amnezia: Xray или WireGuard) wireguard://… или текст .conf WireGuard":
      "https://… (Happ, Incy, Marzban, Remnawave)\nvless://…  vmess://…  trojan://…  ss://…\nvpn://… (Amnezia: Xray or WireGuard)\nwireguard://…  or WireGuard .conf text",
    "Ссылку на подписку даёт VPN-сервис: в личном кабинете или в Telegram-боте, обычно кнопка «Подключить» или «Скопировать ссылку».":
      "Your VPN service gives you the subscription link: in its dashboard or Telegram bot, usually a “Connect” or “Copy link” button.",
    "Из файла…": "From file…", "Подписок пока нет.": "No subscriptions yet.", "Включить/выключить": "On/off", "Удалить": "Delete",
    "Подписка обновлена": "Subscription updated", "Подписка удалена": "Subscription deleted",
    "Вставьте ссылку или ключи": "Paste a link or keys", "Загрузка…": "Loading…",
    // processes
    "Новая группа": "New group", "Только выбранные": "Only selected", "Всё, кроме выбранных": "All except selected",
    "Автоподключение": "Auto-connect",
    "Туннель включится сам, как только запустится программа из списка, и выключится через 15 секунд после того, как все они закроются.":
      "The tunnel turns on by itself when a listed program starts, and off 15 seconds after they all close.",
    "Запущенные": "Running", "Включить группу": "Enable group", "Название группы": "Group name", "Удалить группу": "Delete group",
    "Убрать": "Remove", "Пусто. Добавьте программы справа, через «Обзор» или вручную.": "Empty. Add programs from the right, via “Browse” or by hand.",
    "Сервер группы": "Group server", "Обзор…": "Browse…", "Групп пока нет. Нажмите «Новая группа».": "No groups yet. Click “New group”.",
    "Сначала создайте группу.": "Create a group first.", "Добавить в группу": "Add to group",
    "Ничего не найдено. Запустите программу и нажмите «Обновить».": "Nothing found. Start the program and click “Update”.",
    // terminal
    "Этот компьютер": "This computer", "Мои серверы (SSH)": "My servers (SSH)", "Добавить сервер": "Add server", "Новый сервер": "New server",
    "Название": "Name", "Мой VPS": "My VPS", "Адрес": "Address", "Порт": "Port", "Пользователь": "User", "Вход": "Login",
    "Пароль": "Password", "пароль": "password", "SSH-ключ": "SSH key", "путь к приватному ключу, например id_ed25519": "path to the private key, e.g. id_ed25519",
    "Пароль хранится зашифрованным средствами Windows (DPAPI): прочитать его можно только под вашей учётной записью на этом ПК.":
      "The password is stored encrypted by Windows (DPAPI): only your account on this PC can read it.",
    "Если сервер переустановлен и его ключ сменился": "If the server was reinstalled and its key changed", "Забыть ключ": "Forget key",
    "Отмена": "Cancel", "Сохранить": "Save", "Откройте локальную консоль или подключитесь к своему серверу слева.": "Open a local console or connect to your server on the left.",
    "Терминал работает на": "The terminal runs on", "Подключиться": "Connect", "Изменить": "Edit",
    "Добавьте свой VPS, чтобы подключаться к нему по SSH прямо отсюда.": "Add your VPS to connect to it over SSH right from here.",
    "сохранён (оставьте пустым, чтобы не менять)": "saved (leave empty to keep)", "Пароль от ключа (если есть)": "Key passphrase (if any)",
    "Сервер сохранён": "Server saved", "Ключ сервера забыт: при следующем подключении будет запомнен новый": "Server key forgotten: the next connection will remember the new one",
    // settings
    "Multipath: сколько маршрутов несут каждый пакет": "Multipath: how many routes carry each packet", "1 (выключено)": "1 (off)",
    "Работает для своих relay-серверов: пакет идёт по нескольким маршрутам сразу, доходит первый. VPN-сервер из подписки — это всегда один маршрут.":
      "For your own relays: each packet goes over several routes at once and the first copy wins. A subscription VPN server is always one route.",
    "Переводить в туннель уже открытые соединения": "Move already open connections into the tunnel",
    "Обычно выкидывает из идущего матча. Лучше включать OpenLag до запуска игры.": "Usually drops you from a running match. Better turn OpenLag on before starting the game.",
    "Если ни один сервер не отвечает, трафик выбранных программ блокируется, а не уходит напрямую в обход VPN. Без сервера программы просто не будут работать.":
      "If no server answers, the selected programs' traffic is blocked instead of leaking around the VPN. Without a server they simply won't work.",
    "Обходить TUN других VPN": "Bypass other VPNs' TUN",
    "Если включён Happ, v2rayN и т.п. в режиме TUN, без этого трафик идёт через два VPN подряд: пинг и джиттер растут в разы.":
      "With Happ, v2rayN etc. in TUN mode, traffic would otherwise go through two VPNs in a row: ping and jitter multiply.",
    "User-Agent для загрузки подписок": "User-Agent for subscription downloads", "Некоторые панели отдают разный формат в зависимости от него.": "Some panels return a different format depending on it.",
    "Подключение к VPN-серверам": "Connection to VPN servers", "TLS-отпечаток (uTLS)": "TLS fingerprint (uTLS)", "Как в подписке": "As in the subscription",
    "Случайный": "Random", "Под какой браузер маскируется TLS-рукопожатие с сервером.": "Which browser the TLS handshake with the server imitates.",
    "Фрагментация TLS (обход DPI)": "TLS fragmentation (DPI bypass)",
    "Первый пакет TLS режется на куски, и DPI провайдера не может прочитать имя сайта. Помогает, когда серверы «пропадают» у некоторых провайдеров. Чуть замедляет установку соединения.":
      "The first TLS packet is cut into pieces so the ISP's DPI can't read the site name. Helps when servers “vanish” on some ISPs. Slightly slows down connecting.",
    "Пакеты": "Packets", "Размер куска, байт": "Piece size, bytes", "Пауза, мс": "Pause, ms", "Mux (мультиплексирование TCP)": "Mux (TCP multiplexing)",
    "Выключен (лучше для игр)": "Off (best for games)", "4 потока": "4 streams", "8 потоков": "8 streams", "16 потоков": "16 streams",
    "Несколько TCP-соединений идут внутри одного: меньше рукопожатий для браузера, но для игр лучше выключить. Не применяется к XTLS Vision.":
      "Several TCP connections share one: fewer handshakes for the browser, but keep it off for games. Not used with XTLS Vision.",
    "DNS для адресов серверов": "DNS for server addresses", "Системный": "System", "Яндекс DoH": "Yandex DoH", "Яндекс 77.88.8.8": "Yandex 77.88.8.8",
    "Через что OpenLag узнаёт IP серверов из подписки. DoH прячет запросы от провайдера.": "How OpenLag looks up the IPs of subscription servers. DoH hides the lookups from your ISP.",
    "Цель замера пинга через серверы": "Ping target through servers", "Пинг сервера = время DNS-запроса к этому адресу через сам сервер по UDP.": "Server ping = time of a DNS query to this address through the server over UDP.",
    "Применить": "Apply", "Применено: серверы перезапущены": "Applied: servers restarted",
    "Оформление": "Appearance", "Язык": "Language", "Цвет акцента": "Accent colour", "Изумрудный": "Emerald", "Синий": "Blue", "Фиолетовый": "Violet",
    "Розовый": "Pink", "Янтарный": "Amber", "Красный": "Red", "Бирюзовый": "Cyan", "Длина графиков": "Chart length",
    "1 минута": "1 minute", "2 минуты": "2 minutes", "5 минут": "5 minutes",
    "Цвет пинга: зелёный до, жёлтый до (мс), дальше красный": "Ping colour: green below, yellow below (ms), red above",
    "Компактный режим": "Compact mode", "Меньше отступов, больше строк на экране.": "Less padding, more rows on screen.",
    "Запуск": "Startup", "Запускать вместе с Windows": "Start with Windows", "Через Планировщик задач с правами администратора, без окна UAC при входе.": "Via Task Scheduler with administrator rights, no UAC prompt at sign-in.",
    "Запускать свёрнутым": "Start minimized", "Крестик сворачивает в трей": "Close button minimizes to tray",
    "OpenLag продолжит работать: иконка у часов, левый клик открывает окно, правый даёт меню с переключателем и выбором сервера. «Выход» в меню трея закрывает программу совсем.":
      "OpenLag keeps running: an icon by the clock, left click opens the window, right click gives a menu with the switch and server choice. “Quit” in the tray menu closes it for good.",
    "Проверять обновления": "Check for updates", "Раз в 6 часов OpenLag смотрит последний релиз на GitHub. Обновление ставится только по вашей кнопке.": "Every 6 hours OpenLag looks at the latest release on GitHub. Updates install only when you click.",
    "Проверить сейчас": "Check now", "Частота замера пинга своих relay": "Ping rate for own relays",
    "10 раз в секунду": "10 times a second", "4 раза в секунду": "4 times a second", "2 раза в секунду": "twice a second", "раз в секунду": "once a second",
    "Свои relay-серверы": "Own relay servers",
    "Есть VPS с Linux? OpenLag сам поставит на него relay — свой выходной узел рядом с игровыми серверами. Нужен вход по SSH под root или с sudo: сервер добавляется во вкладке «Терминал».":
      "Have a Linux VPS? OpenLag installs a relay on it for you — your own exit node next to the game servers. Needs SSH login as root or with sudo: add the server on the “Terminal” tab.",
    "Название в OpenLag": "Name in OpenLag", "Франкфурт": "Frankfurt", "UDP-порт": "UDP port", "Удалить с сервера": "Remove from server",
    "Установить relay": "Install relay", "Вручную (TOML): ключ и маршруты, в том числе через мосты": "Manually (TOML): key and routes, bridges included",
    "Сохранить relay": "Save relays",
    'psk = "секрет"\n\n[[exits]]\nname = "Франкфурт"\nroutes = [\n  { name = "direct", via = "203.0.113.10:7000" },\n  { name = "bridge", via = "198.51.100.20:7001" },\n]':
      'psk = "secret"\n\n[[exits]]\nname = "Frankfurt"\nroutes = [\n  { name = "direct", via = "203.0.113.10:7000" },\n  { name = "bridge", via = "198.51.100.20:7001" },\n]', "Состояние": "Status", "Сохранено": "Saved", "Оформление сохранено": "Appearance saved",
    "Сначала добавьте сервер во вкладке «Терминал»": "Add a server on the “Terminal” tab first",
    "Kill switch включён: без сервера трафик выбранных программ блокируется": "Kill switch on: without a server the selected programs' traffic is blocked",
    "Kill switch выключен": "Kill switch off", "Порт должен быть от 1024 до 65535": "The port must be 1024–65535",
    "Установка relay": "Installing relay", "Удаление relay": "Removing relay", "Relay-серверы обновлены": "Relays updated", "Relay-серверы сохранены": "Relays saved",
    "Права администратора": "Administrator rights", "Выход к VPN-серверам": "Route to VPN servers", "по таблице маршрутов": "by the routing table",
    "Перехват": "Capture", "работает": "running", "выключен": "off", "есть": "yes", "нет": "no", "Администратор": "Administrator",
    "Без прав администратора": "No administrator rights",
    // fun
    "пока пинг грузится": "while the ping loads", "Калькулятор": "Calculator", "Змейка": "Snake", "Счёт": "Score", "· Рекорд": "· Best",
    "Змейка: стрелки или WASD, пробел — пауза": "Snake: arrows or WASD, space pauses", "Стрелки или WASD, пробел — пауза": "Arrows or WASD, space pauses",
    "Играть": "Play", "Стереть": "Clear", "Пауза": "Paused", "Ещё раз": "Again", "Заново": "Restart", "Ошибка": "Error", "сейчас": "now",
    // errors from the backend (already Russian-ised by ru())
    "Для перехвата нужны права администратора": "Capture needs administrator rights",
    "Выберите хотя бы один процесс на вкладке «Процессы»": "Select at least one process on the “Processes” tab",
    "Сначала добавьте подписку или свой relay-сервер": "Add a subscription or your own relay first",
    "Права администратора не выданы": "Administrator rights were not granted",
    "только что": "just now",
    // tray-like statuses and defaults
    "Игры": "Games", "Мои ключи": "My keys", "Через VPN": "Through VPN", "Не через VPN": "Not through VPN", "Группа": "Group",
    "Белые списки": "Whitelists", "Торренты": "Torrents",
  };
  const EN = new Map(Object.entries(EN_RAW).map(([k, v]) => [norm(k), v]));

  // Phrases with values inside. `$1`... are translated recursively when they are
  // phrases themselves (like "обновлено 5 мин назад").
  const PATTERNS = [
    [/^([\d.,]+) мс$/, "$1 ms"],
    [/^([\d.,]+) с назад$/, "$1 s ago"],
    [/^(\d+) мин назад$/, "$1 min ago"],
    [/^(\d+) ч назад$/, "$1 h ago"],
    [/^([\d.,]+) (Б|КБ|МБ|ГБ|ТБ)$/, (m, n, u) => `${n} ${{ "Б": "B", "КБ": "KB", "МБ": "MB", "ГБ": "GB", "ТБ": "TB" }[u]}`],
    [/^(.+) → (.+) мс$/, "$1 → $2 ms"],
    [/^джиттер (.+) → (.+) мс · серверов: (\d+)$/, "jitter $1 → $2 ms · servers: $3"],
    [/^показано 30 из (\d+) TCP$/, "showing 30 of $1 TCP"],
    [/^Доступна новая версия OpenLag (.+) \(у вас (.+)\)\.$/, "OpenLag $1 is available (you have $2)."],
    [/^Через туннель: (.+)$/, "Through the tunnel: $1"],
    [/^Будет через туннель: (.+)$/, "Will go through the tunnel: $1"],
    [/^весь трафик, кроме (\d+) процесс\.$/, "all traffic except $1 processes"],
    [/^весь трафик$/, "all traffic"],
    [/^(.+) и ещё (\d+)$/, "$1 and $2 more"],
    [/^(.+) · авто$/, "$1 · auto"],
    [/^(.+): включён$/, "$1: on"], [/^(.+): отключён$/, "$1: off"],
    [/^(.+): включена$/, "$1: on"], [/^(.+): отключена$/, "$1: off"],
    [/^Новые соединения пойдут через (.+)$/, "New connections will go through $1"],
    [/^(.*)выход (\S+)(.*)$/, (m, a, ip, b) => `${a}exit ${ip}${b}`],
    [/^Отключён · (.+)$/, "Off · $1"],
    [/^отключён вручную$/, "switched off by hand"],
    [/^авто-серверы отключены$/, "auto servers switched off"],
    [/^страна (.+) отключена$/, "country $1 switched off"],
    [/^фильтр «(.+)»$/, "filter “$1”"],
    [/^Включить все серверы: (.+)$/, "Turn on all servers: $1"],
    [/^Отключить все серверы: (.+)$/, "Turn off all servers: $1"],
    [/^· не определено: (\d+)$/, "· not detected: $1"],
    [/^отключено серверов: (\d+) из (\d+)$/, "servers off: $1 of $2"],
    [/^(\d+) серв\. · обновлено (.+)$/, "$1 servers · updated $2"],
    [/^(\d+) серв\. · добавлено вручную$/, "$1 servers · added by hand"],
    [/^Трафик: (.+)$/, "Traffic: $1"],
    [/^До: (.+)$/, "Until: $1"],
    [/^Пропущено: (\d+)$/, "Skipped: $1"],
    [/^«(.+)»: добавлено серверов (\d+), пропущено (\d+)$/, "“$1”: $2 servers added, $3 skipped"],
    [/^«(.+)»: добавлено серверов (\d+)$/, "“$1”: $2 servers added"],
    [/^Через туннель идёт весь трафик компьютера, кроме процессов из включённых групп ниже \((\d+)\)\. Локальная сеть всегда идёт напрямую\. У этого режима свой список\.$/,
      "All of the computer's traffic goes through the tunnel except the processes of the enabled groups below ($1). The local network always goes direct. This mode has its own list."],
    [/^Через туннель идут только процессы из включённых групп ниже \((\d+)\)\. Остальное работает напрямую, без VPN и без TUN\. У этого режима свой список\.$/,
      "Only the processes of the enabled groups below ($1) go through the tunnel. Everything else stays direct, with no VPN and no TUN. This mode has its own list."],
    [/^Как выбрано в «Серверах» \((.+)\)$/, "As chosen in “Servers” ($1)"],
    [/^авто$/, "auto"],
    [/^Свой relay · (.+)$/, "Own relay · $1"],
    [/^Недоступен: (.+)$/, "Unavailable: $1"],
    [/^Группа (\d+)$/, "Group $1"],
    [/^Версия (.+) · доступна (.+)$/, "Version $1 · $2 available"],
    [/^Версия (.+)$/, "Version $1"],
    [/^(.+) \(в обход чужих TUN\)$/, "$1 (bypassing other TUNs)"],
    [/^Скачиваю OpenLag (.*)…$/, "Downloading OpenLag $1…"],
    [/^Загрузка (\d+)%$/, "Downloading $1%"],
    [/^Доступна версия (.+)$/, "Version $1 is available"],
    [/^У вас последняя версия \((.+)\)$/, "You have the latest version ($1)"],
    [/^Не удалось проверить обновления: (.+)$/, "Could not check for updates: $1"],
    [/^Не удалось открыть терминал: (.+)$/, "Could not open the terminal: $1"],
    [/^Сервер «(.+)»$/, "Server “$1”"],
    [/^Конец игры: (\d+)$/, "Game over: $1"],
    [/^→ «(.+)»$/, "→ “$1”"],
    [/^Запущен (.+): туннель включён$/, "$1 started: tunnel on"],
    [/^Программы из списка закрыты: туннель выключен$/, "Listed programs closed: tunnel off"],
    [/^Не удалось включить: (.+)$/, "Could not turn on: $1"],
    [/^Сервер подписки ответил ошибкой HTTP (\d+)$/, "The subscription server answered HTTP $1"],
    // Traffic "1.2 ГБ из 50 ГБ"; kept last so longer sentences with "из" match first.
    [/^([\d.,]+ \S+) из ([\d.,]+ \S+)$/, "$1 of $2"],
  ];

  let lang = "ru";
  try { lang = localStorage.getItem("openlag-lang") || (navigator.language.startsWith("ru") ? "ru" : "en"); } catch { /* private mode */ }

  function translate(text) {
    const key = norm(text);
    if (!key || !/[А-Яа-яЁё]/.test(key)) return null;
    const hit = EN.get(key);
    if (hit != null) return hit;
    for (const [re, to] of PATTERNS) {
      const m = key.match(re);
      if (!m) continue;
      if (typeof to === "function") return to(...m);
      return to.replace(/\$(\d)/g, (_, i) => { const g = m[+i] ?? ""; return translate(g) ?? g; });
    }
    return null;
  }

  const ATTRS = ["placeholder", "title", "aria-label"];
  function translateNode(n) {
    if (n.nodeType === 3) {
      const p = n.parentNode;
      if (!p || p.nodeName === "SCRIPT" || p.nodeName === "STYLE" || p.nodeName === "TEXTAREA") return;
      const en = translate(n.nodeValue);
      if (en != null) {
        // Keep surrounding spaces: they separate inline elements.
        const lead = n.nodeValue.match(/^\s*/)[0], trail = n.nodeValue.match(/\s*$/)[0];
        n.nodeValue = lead + en + trail;
      }
    } else if (n.nodeType === 1) {
      for (const a of ATTRS) {
        const v = n.getAttribute(a);
        if (v) { const en = translate(v); if (en != null) n.setAttribute(a, en); }
      }
      if (n.closest?.(".xterm")) return;
      for (const c of n.childNodes) translateNode(c);
    }
  }

  let observer = null;
  function start() {
    if (lang !== "en" || observer) return;
    document.documentElement.lang = "en";
    translateNode(document.body);
    observer = new MutationObserver((list) => {
      for (const m of list) {
        if (m.type === "characterData") translateNode(m.target);
        else if (m.type === "attributes") translateNode(m.target);
        else m.addedNodes.forEach(translateNode);
      }
    });
    observer.observe(document.body, { subtree: true, childList: true, characterData: true, attributes: true, attributeFilter: ATTRS });
  }

  return {
    get lang() { return lang; },
    /** Explicit translation for canvas and terminal text. */
    t(ru) { return lang === "en" ? (translate(ru) ?? ru) : ru; },
    start,
    preview(l) { lang = l; },
    /** More phrases (the installer has its own); its patterns are tried first. */
    add(raw, patterns = []) {
      for (const [k, v] of Object.entries(raw)) EN.set(norm(k), v);
      PATTERNS.unshift(...patterns);
    },
    /** Switches language; the page reloads so everything renders fresh. */
    set(l) {
      try { localStorage.setItem("openlag-lang", l); } catch { /* ignore */ }
      if (l !== lang) location.reload();
    },
    /** The saved setting wins over the browser guess on first start. */
    sync(l) {
      if (!l || l === lang) return;
      this.set(l);
    },
  };
})();
