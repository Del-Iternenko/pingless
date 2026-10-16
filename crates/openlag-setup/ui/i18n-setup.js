// English for the installer, on top of the shared translator (i18n.js, copied from
// the app at build time).
"use strict";

I18N.add({
  "Установка Pingless": "Pingless Setup", "Свернуть": "Minimize", "Закрыть": "Close",
  "Пинг ниже. Игра чище.": "Lower ping. Cleaner game.", "Сейчас играет": "Now playing", "Династия": "Dynasty",
  "Выключить музыку": "Mute music", "Громкость": "Volume",
  "Добро пожаловать": "Welcome", "Мастер установит": "This wizard installs",
  ", open-source оптимизатор игрового трафика: игры, браузер или что угодно пойдут через ваши VPN-подписки или свои серверы, по нескольким маршрутам сразу и без TUN.":
    ", an open-source game traffic optimizer: games, the browser or anything else go through your VPN subscriptions or your own servers, over several routes at once and without TUN.",
  "пакет летит по 2–4 маршрутам, доходит первый": "each packet takes 2–4 routes, the first one wins",
  "По процессам": "Per process", "через туннель только то, что вы выбрали": "only what you choose goes through the tunnel",
  "Подписки": "Subscriptions", "Живой пинг": "Live ping", "графики пинга, джиттера и потерь": "ping, jitter and loss charts",
  "Далее": "Next", "Назад": "Back",
  "Пользовательское соглашение": "License agreement",
  "Pingless — свободное программное обеспечение": "Pingless is free software",
  ", распространяется по лицензиям MIT или Apache-2.0 на выбор пользователя.": ", distributed under the MIT or Apache-2.0 license at your option.",
  "1. Программа предоставляется «как есть», без каких-либо гарантий. Автор не отвечает за прямой или косвенный ущерб, потерю данных, блокировки игровых аккаунтов или нарушения правил сервисов, возникшие при её использовании.":
    "1. The software is provided “as is”, without warranty of any kind. The author is not liable for any direct or indirect damage, data loss, game account bans or breaches of service rules arising from its use.",
  "2. Pingless перехватывает сетевой трафик выбранных программ с помощью драйвера WinDivert и направляет его через серверы, которые указали вы: свои relay-серверы или серверы ваших VPN-подписок. Автор Pingless не предоставляет серверов и не видит ваш трафик.":
    "2. Pingless captures the network traffic of the programs you select with the WinDivert driver and sends it through servers you specify: your own relays or your VPN subscriptions' servers. The author of Pingless provides no servers and never sees your traffic.",
  "3. Вы самостоятельно отвечаете за законность использования VPN и прокси в вашей стране, а также за соблюдение условий ваших VPN-провайдеров и правил игр. Некоторые античиты могут не допускать драйверы перехвата трафика.":
    "3. You are responsible for the legality of using VPNs and proxies in your country and for following your VPN providers' terms and the games' rules. Some anti-cheats may not allow traffic capture drivers.",
  "4. Вместе с программой устанавливаются сторонние компоненты: WinDivert (LGPL-3.0 / GPL-2.0, © basil00), xray-core (MPL-2.0, © Project X) и, при необходимости, Microsoft Edge WebView2 Runtime. Тексты лицензий лежат в папке программы, в каталоге licenses.":
    "4. Third-party components are installed along with it: WinDivert (LGPL-3.0 / GPL-2.0, © basil00), xray-core (MPL-2.0, © Project X) and, if needed, Microsoft Edge WebView2 Runtime. Their licenses are in the licenses folder of the program.",
  "5. Программа хранит настройки и подписки только на вашем компьютере, в папке %APPDATA%\\dev.openlag.app, и никуда их не отправляет, кроме загрузки самих подписок с адресов, которые вы добавили.":
    "5. Settings and subscriptions stay on your computer, in %APPDATA%\\dev.openlag.app, and are sent nowhere, apart from downloading the subscriptions from the addresses you added.",
  "6. Для работы перехвата программе нужны права администратора. Удалить Pingless можно через «Параметры → Приложения» или uninstall.exe в папке программы.":
    "6. Traffic capture needs administrator rights. Uninstall Pingless from “Settings → Apps” or with uninstall.exe in its folder.",
  "Исходный код:": "Source code:", "Я принимаю условия пользовательского соглашения": "I accept the license agreement",
  "Параметры установки": "Install options", "Папка установки": "Install folder", "Обзор…": "Browse…",
  "Создать ярлык на рабочем столе": "Create a desktop shortcut", "Добавить в меню «Пуск»": "Add to the Start menu",
  "Дополнительное ПО": "Additional software",
  "WinDivert (драйвер перехвата), xray-core (подписки VLESS/VMess/Trojan/SS) и WebView2 Runtime, если его нет. Без него Pingless не сможет перехватывать трафик.":
    "WinDivert (capture driver), xray-core (VLESS/VMess/Trojan/SS/WireGuard subscriptions) and WebView2 Runtime if missing. Without them Pingless cannot capture traffic.",
  "Запускать Pingless вместе с Windows": "Start Pingless with Windows", "Запустить Pingless после установки": "Launch Pingless after setup",
  "Установить": "Install", "Установка…": "Installing…", "Pingless установлен": "Pingless is installed",
  "Всё готово. Добавьте подписку на вкладке «Подписки», выберите программы на вкладке «Процессы» и жмите большую кнопку.":
    "All set. Add a subscription on the “Subscriptions” tab, pick programs on the “Processes” tab and hit the big button.",
  "исходники, обновления, звёздочки приветствуются": "source, updates; stars are welcome", "Готово": "Done",
  "Удаление Pingless": "Uninstall Pingless",
  "Pingless будет удалён вместе с ярлыками, автозапуском и правилом брандмауэра. Это займёт пару секунд.":
    "Pingless will be removed together with its shortcuts, autostart and firewall rule. It takes a couple of seconds.",
  "Удалить также настройки и подписки": "Also delete settings and subscriptions", "Отмена": "Cancel", "Удалить": "Uninstall",
  "Приветствие": "Welcome", "Соглашение": "Agreement", "Параметры": "Options", "Установка": "Install", "Обновление": "Update",
  "Удаление": "Uninstall", "Процесс": "Progress",
  "Совет: включите «Автоподключение» на вкладке «Процессы», и туннель будет подниматься сам, как только вы запустите игру.":
    "Tip: turn on “Auto-connect” on the “Processes” tab and the tunnel comes up by itself as soon as you start a game.",
  "Совет: фильтры серверов отключают целые страны одним словом, например «Russia», даже после обновления подписки.":
    "Tip: server filters switch off whole countries with one word, like “Russia”, even after the subscription updates.",
  "Совет: multipath отправляет каждый пакет по нескольким маршрутам, и потери на одном из них игра просто не заметит.":
    "Tip: multipath sends every packet over several routes, so the game never notices loss on one of them.",
  "Совет: режим «Всё, кроме выбранных» пустит через VPN весь компьютер, кроме программ из списка.":
    "Tip: the “All except selected” mode sends the whole computer through the VPN except the listed programs.",
  "Совет: включён Happ или другой VPN в режиме TUN? Pingless обойдёт его, чтобы трафик не шёл через два VPN подряд.":
    "Tip: Happ or another VPN in TUN mode? Pingless bypasses it so traffic doesn't go through two VPNs in a row.",
  "Совет: на вкладке «Фан» есть змейка. Просто чтобы было.": "Tip: there is a snake game on the “Fun” tab. Just because.",
  "Без дополнительного ПО Pingless не сможет перехватывать трафик: ставьте только если WinDivert и xray уже лежат в папке программы.":
    "Without the additional software Pingless cannot capture traffic: skip it only if WinDivert and xray are already in the program folder.",
  "Укажите папку установки": "Choose the install folder",
  "Настройки, подписки и группы на месте. Pingless сейчас запустится.": "Settings, subscriptions and groups are kept. Pingless starts now.",
  "Удаление…": "Uninstalling…",
  "Спасибо, что попробовали Pingless. Будем рады issue и звёздочке на GitHub.": "Thanks for trying Pingless. Issues and stars on GitHub are welcome.",
  "Pingless удалён": "Pingless is uninstalled", "Программа, ярлыки и настройки удалены.": "The program, shortcuts and settings are removed.",
  "Программа и ярлыки удалены, настройки и подписки сохранены на случай, если вернётесь.": "The program and shortcuts are removed; settings and subscriptions are kept in case you come back.",
  // progress lines from the installer backend
  "Закрываю запущенный Pingless…": "Closing running Pingless…", "Устанавливаю WebView2 Runtime…": "Installing WebView2 Runtime…",
  "Регистрирую программу…": "Registering the program…", "Создаю ярлыки…": "Creating shortcuts…", "Закрываю Pingless…": "Closing Pingless…",
  "Убираю автозапуск и правило брандмауэра…": "Removing autostart and the firewall rule…", "Удаляю ярлыки…": "Removing shortcuts…",
  "Удаляю файлы…": "Removing files…", "укажите полный путь к папке установки": "enter the full path of the install folder",
  "Pingless не найден": "Pingless is not found",
}, [
  [/^Потребуется на диске: (\d+) МБ$/, "Disk space needed: $1 MB"],
  [/^Потребуется на диске: (\d+) МБ · найдена прежняя установка, она будет обновлена$/, "Disk space needed: $1 MB · an existing install was found and will be updated"],
  [/^Установка не удалась: (.+)$/, "Setup failed: $1"],
  [/^Обновление до (.+)…$/, "Updating to $1…"],
  [/^Pingless обновлён до (.+)$/, "Pingless is updated to $1"],
  [/^Обновление не удалось: (.+)$/, "Update failed: $1"],
  [/^Удаление не удалось: (.+)$/, "Uninstall failed: $1"],
  [/^Распаковка (.+)$/, "Unpacking $1"],
  [/^не удалось создать (.+)$/, "cannot create $1"],
  [/^не удалось записать (.+)$/, "cannot write $1"],
]);
I18N.start();
{
  const b = document.getElementById("btn-lang");
  b.textContent = I18N.lang === "en" ? "RU" : "EN";
  b.addEventListener("click", () => I18N.set(I18N.lang === "en" ? "ru" : "en"));
}
