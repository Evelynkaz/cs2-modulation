// Every Russian UI string lives here, so a later pass never has to hunt through the other
// modules for text to fix (`s6f1_viewer_shell.md`: "русские строки — в одном месте").

export const strings = {
  appTitle: "cs2-modulation",

  setup: {
    heading: "Первый запуск",
    intro: "Программа читает только файлы на диске и ничего не делает с запущенной игрой.",
    gameDirLabel: "Папка CS2",
    gameDirHint: "Подойдёт и корень установки, и путь вида ...\\game\\csgo",
    gameDirPlaceholder: "C:\\Program Files (x86)\\Steam\\steamapps\\common\\Counter-Strike Global Offensive",
    checkButton: "Проверить",
    checking: "Проверяю...",
    gameDirAdjusted: (dir) => `Сохранён путь: ${dir}`,
    gameBuildFound: (build) => `Найден билд игры: ${build}`,
    cacheDirLabel: "Папка кэша (необязательно)",
    cacheDirHint: "Куда сохранять извлечённые данные карт; по умолчанию рядом с программой.",
    cacheDirPlaceholder: "оставьте пустым, чтобы использовать путь по умолчанию",
    saveCacheButton: "Сохранить",
    restartRequired: "Новый путь к кэшу заработает после перезапуска cs2mod serve.",
    cacheDirSaved: "Сохранено.",
    continueButton: "Продолжить",
  },

  maps: {
    heading: "Карты",
    backToSetup: "Настройки",
    columnMap: "Карта",
    columnBuild: "Билд",
    geometry: "геометрия",
    standSpots: "точки стояния",
    radar: "радар",
    staleNoGameDir: "кэш не сверен с установленной игрой",
    staleWrongBuild: "кэш от другого билда игры",
    reextractButton: "Переизвлечь",
    prepareButton: "Подготовить",
    cancelButton: "Отменить",
    openButton: "Открыть",
    noMaps: "Нет ни одной подготовленной карты.",
    extractNewLabel: "Извлечь карту",
    extractNamePlaceholder: "de_mirage",
    extractButton: "Извлечь",
    stageExtract: "извлечение",
    stageStandspots: "точки стояния",
    stageViewerdata: "радар",
    stageQueued: "в очереди",
    stageDone: "готово",
    loading: "Загрузка карт...",
    pillOk: "✓",
    pillMissing: "—",
  },

  mapScreen: {
    backToList: "к списку карт",
    targetComingSoon: "выбор цели появится в следующей части",
  },

  errors: {
    serverDown: "Сервер cs2mod serve не отвечает.",
    streamBroken: "Соединение с задачей прервалось. Переподключиться?",
    reconnectButton: "Переподключиться",
    retryButton: "Повторить",
    genericPrefix: "Ошибка",
  },

  theme: {
    toggleToDark: "Тёмная тема",
    toggleToLight: "Светлая тема",
  },
};
