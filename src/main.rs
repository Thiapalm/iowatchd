//! iowatchd
//!
//! Monitora o percentual de uso de IO de um disco (via `/proc/diskstats`) e,
//! quando ele fica acima do limite configurado, registra quais processos estao
//! gerando mais leitura/escrita (via `/proc/<pid>/io`).
//!
//! Principios de projeto (consumo minimo de recursos):
//! * Sem dependencias externas e sem runtime assincrono.
//! * O loop principal le um unico arquivo pequeno e dorme (`nap`)
//!   entre amostras: CPU praticamente zero.
//! * A varredura de processos, que e a parte "cara", so acontece quando o
//!   limite e ultrapassado.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Configuracao
// ---------------------------------------------------------------------------

/// Parametros do monitor. Valores padrao em [`Config::default`]; podem ser
/// sobrescritos por um arquivo `chave = valor` (veja `config.conf`).
#[derive(Debug, Clone, PartialEq)]
struct Config {
    /// Nome do dispositivo em `/proc/diskstats` (ex.: `sda`, `nvme0n1`) ou
    /// `auto` para detectar o disco fisico que contem a particao raiz.
    device: String,
    /// Uso de IO (%) a partir do qual os processos sao registrados.
    threshold: f64,
    /// Uso de IO (%) abaixo do qual consideramos o IO normalizado.
    /// Ser menor que `threshold` evita ficar alternando entre os estados.
    clear_threshold: f64,
    /// Segundos entre amostras do disco.
    interval: u64,
    /// Amostras consecutivas acima do limite necessarias para disparar.
    trigger_samples: u32,
    /// Janela (s) para medir o IO por processo quando disparado.
    process_window: u64,
    /// Quantos processos listar por registro.
    top_n: usize,
    /// Intervalo minimo (s) entre registros enquanto o IO segue alto.
    cooldown: u64,
    /// Arquivo de log adicional ao stdout/journald (`None` desativa).
    log_file: Option<PathBuf>,
    /// Tamanho maximo do arquivo de log (bytes) antes de rotacionar.
    log_max_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device: "auto".into(),
            threshold: 80.0,
            clear_threshold: 70.0,
            interval: 2,
            trigger_samples: 2,
            process_window: 1,
            top_n: 5,
            cooldown: 10,
            log_file: Some(PathBuf::from("/var/log/iowatchd.log")),
            log_max_bytes: 5 * 1024 * 1024,
        }
    }
}

impl Config {
    /// Aplica o conteudo de um arquivo de configuracao sobre os padroes.
    /// Linhas invalidas sao ignoradas e devolvidas como avisos.
    fn parse(text: &str) -> (Config, Vec<String>) {
        let mut cfg = Config::default();
        let mut warnings = Vec::new();

        for (n, raw) in text.lines().enumerate() {
            // Remove comentarios e espacos.
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warnings.push(format!("linha {}: esperado 'chave = valor'", n + 1));
                continue;
            };
            let (key, value) = (key.trim(), value.trim());

            let ok = match key {
                "device" => {
                    cfg.device = value.to_string();
                    !value.is_empty()
                }
                "threshold" => value.parse().map(|v| cfg.threshold = v).is_ok(),
                "clear_threshold" => value.parse().map(|v| cfg.clear_threshold = v).is_ok(),
                "interval" => value.parse().map(|v| cfg.interval = v).is_ok(),
                "trigger_samples" => value.parse().map(|v| cfg.trigger_samples = v).is_ok(),
                "process_window" => value.parse().map(|v| cfg.process_window = v).is_ok(),
                "top_n" => value.parse().map(|v| cfg.top_n = v).is_ok(),
                "cooldown" => value.parse().map(|v| cfg.cooldown = v).is_ok(),
                "log_file" => {
                    cfg.log_file = if value.is_empty() { None } else { Some(value.into()) };
                    true
                }
                "log_max_mb" => value
                    .parse::<u64>()
                    .map(|v| cfg.log_max_bytes = v * 1024 * 1024)
                    .is_ok(),
                _ => {
                    warnings.push(format!("linha {}: chave desconhecida '{}'", n + 1, key));
                    true
                }
            };
            if !ok {
                warnings.push(format!("linha {}: valor invalido para '{}'", n + 1, key));
            }
        }

        cfg.normalize();
        (cfg, warnings)
    }

    /// Garante valores seguros (evita loop sem espera ou histerese invertida).
    fn normalize(&mut self) {
        self.interval = self.interval.max(1);
        self.process_window = self.process_window.max(1);
        self.trigger_samples = self.trigger_samples.max(1);
        if self.clear_threshold > self.threshold {
            self.clear_threshold = self.threshold;
        }
    }
}

// ---------------------------------------------------------------------------
// Log (stdout/journald + arquivo com rotacao simples)
// ---------------------------------------------------------------------------

struct Logger {
    file: Option<PathBuf>,
    max_bytes: u64,
}

impl Logger {
    /// Escreve uma linha no stdout (capturado pelo journald, que ja
    /// adiciona data/hora) e no arquivo de log (com data/hora propria).
    fn log(&self, msg: &str) {
        println!("{msg}");
        let Some(path) = &self.file else { return };

        // Rotacao: ao passar do limite, o arquivo atual vira `<arquivo>.1`.
        if fs::metadata(path).map(|m| m.len() > self.max_bytes).unwrap_or(false) {
            let mut old = path.clone().into_os_string();
            old.push(".1");
            let _ = fs::rename(path, old);
        }

        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{} {}", timestamp(), msg);
            }
            Err(e) => eprintln!("aviso: nao foi possivel escrever em {}: {e}", path.display()),
        }
    }
}

/// Estrutura `struct tm` da libc (layout de Linux 64 bits).
#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
    gmtoff: i64,
    zone: *const i8,
}

extern "C" {
    fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
}

/// Data/hora local no formato `YYYY-MM-DD HH:MM:SS`.
/// Usa `localtime_r` da libc (ja linkada pela std) para respeitar o fuso
/// horario do sistema sem precisar de bibliotecas externas.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut tm = Tm {
        sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0,
        wday: 0, yday: 0, isdst: 0, gmtoff: 0, zone: std::ptr::null(),
    };
    // SAFETY: `tm` e valido e gravavel durante a chamada; `secs` e lido apenas.
    let ok = unsafe { !localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return secs.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.year + 1900, tm.mon + 1, tm.mday, tm.hour, tm.min, tm.sec
    )
}

// ---------------------------------------------------------------------------
// Deteccao automatica do disco
// ---------------------------------------------------------------------------

/// Procura em `/proc/self/mountinfo` a origem (ex.: `/dev/mapper/vg-root`)
/// do sistema de arquivos montado em `/`. Se houver varias montagens em `/`,
/// vale a ultima (a que esta por cima).
fn parse_root_source(mountinfo: &str) -> Option<String> {
    mountinfo
        .lines()
        .filter_map(|line| {
            // Formato: "... ponto_de_montagem opcoes... - tipo origem opcoes"
            let (head, tail) = line.split_once(" - ")?;
            if head.split_whitespace().nth(4)? != "/" {
                return None;
            }
            let source = tail.split_whitespace().nth(1)?;
            source.starts_with("/dev/").then(|| source.to_string())
        })
        .last()
}

/// Dado o nome de um dispositivo de bloco (particao, LVM, LUKS...), devolve
/// o disco fisico de baixo, usando a hierarquia do sysfs:
/// * dispositivos empilhados (dm, md) listam seus componentes em `slaves/`;
/// * particoes tem o arquivo `partition` e ficam dentro do diretorio do disco.
fn physical_disk(name: &str, depth: u32) -> Option<String> {
    let sys = PathBuf::from("/sys/class/block").join(name);
    if depth > 8 || !sys.exists() {
        return None;
    }
    let first_slave = fs::read_dir(sys.join("slaves"))
        .ok()
        .and_then(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).min());
    if let Some(slave) = first_slave {
        return physical_disk(&slave, depth + 1);
    }
    if sys.join("partition").exists() {
        let real = fs::canonicalize(&sys).ok()?;
        return real.parent()?.file_name()?.to_str().map(String::from);
    }
    Some(name.to_string())
}

/// Detecta o disco a monitorar: primeiro o que contem `/`; se nao for
/// possivel (ex.: overlayfs), o primeiro disco com hardware real.
fn detect_device() -> Option<String> {
    let from_root = fs::read_to_string("/proc/self/mountinfo")
        .ok()
        .and_then(|m| parse_root_source(&m))
        .and_then(|src| fs::canonicalize(src).ok()) // /dev/mapper/x -> /dev/dm-0
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .and_then(|n| physical_disk(&n, 0));
    if from_root.is_some() {
        return from_root;
    }
    // Discos reais tem `device/`; loop, ram e zram sao virtuais e nao tem.
    let mut disks: Vec<String> = fs::read_dir("/sys/block")
        .ok()?
        .flatten()
        .filter(|e| e.path().join("device").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    disks.sort();
    disks.into_iter().next()
}

// ---------------------------------------------------------------------------
// Uso de IO do disco
// ---------------------------------------------------------------------------

/// Extrai de `/proc/diskstats` o campo `io_ticks` (ms em que o disco teve IO
/// em andamento) do dispositivo informado.
///
/// Formato de cada linha: `major minor nome campo1 campo2 ...`, sendo
/// `io_ticks` o 10o campo de estatistica, ou seja, o indice 12 da linha.
fn parse_io_ticks(diskstats: &str, device: &str) -> Option<u64> {
    diskstats.lines().find_map(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.get(2) == Some(&device) {
            parts.get(12)?.parse().ok()
        } else {
            None
        }
    })
}

fn read_io_ticks(device: &str) -> Option<u64> {
    parse_io_ticks(&fs::read_to_string("/proc/diskstats").ok()?, device)
}

/// Percentual de uso (equivalente ao `%util` do `iostat -x`):
/// fracao do tempo decorrido em que o disco esteve ocupado.
fn utilization(delta_ticks_ms: u64, elapsed_ms: u64) -> f64 {
    if elapsed_ms == 0 {
        return 0.0;
    }
    (delta_ticks_ms as f64 / elapsed_ms as f64 * 100.0).min(100.0)
}

// ---------------------------------------------------------------------------
// IO por processo
// ---------------------------------------------------------------------------

/// Bytes lidos/escritos de fato no armazenamento por um processo
/// (`read_bytes` e `write_bytes` de `/proc/<pid>/io`; ignora cache de pagina).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct ProcIo {
    read: u64,
    write: u64,
}

/// Le os contadores de IO de um PID. Retorna `None` se o processo terminou
/// ou se nao temos permissao.
fn read_proc_io(pid: u32) -> Option<ProcIo> {
    parse_proc_io(&fs::read_to_string(format!("/proc/{pid}/io")).ok()?)
}

fn parse_proc_io(text: &str) -> Option<ProcIo> {
    let mut io = ProcIo::default();
    let mut found = false;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            io.read = v.trim().parse().ok()?;
            found = true;
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            io.write = v.trim().parse().ok()?;
        }
    }
    found.then_some(io)
}

/// Tira uma "foto" dos contadores de IO de todos os processos visiveis.
fn snapshot_processes() -> HashMap<u32, ProcIo> {
    let mut map = HashMap::new();
    let Ok(dir) = fs::read_dir("/proc") else { return map };
    for entry in dir.flatten() {
        // Diretorios numericos em /proc correspondem a PIDs.
        if let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) {
            if let Some(io) = read_proc_io(pid) {
                map.insert(pid, io);
            }
        }
    }
    map
}

/// Consumo de um processo durante a janela de medicao.
struct ProcUsage {
    pid: u32,
    read: u64,
    write: u64,
}

/// Compara duas fotos e devolve os `n` processos que mais fizeram IO,
/// em ordem decrescente (leitura + escrita).
fn top_processes(before: &HashMap<u32, ProcIo>, after: &HashMap<u32, ProcIo>, n: usize) -> Vec<ProcUsage> {
    let mut usage: Vec<ProcUsage> = after
        .iter()
        .map(|(&pid, now)| {
            // Processo novo na janela: parte de zero.
            let old = before.get(&pid).copied().unwrap_or_default();
            ProcUsage {
                pid,
                read: now.read.saturating_sub(old.read),
                write: now.write.saturating_sub(old.write),
            }
        })
        .filter(|u| u.read + u.write > 0)
        .collect();
    usage.sort_by(|a, b| (b.read + b.write).cmp(&(a.read + a.write)));
    usage.truncate(n);
    usage
}

/// Nome curto do processo (`/proc/<pid>/comm`).
fn process_name(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Linha de comando completa, truncada para nao poluir o log.
fn process_cmdline(pid: u32) -> String {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let text: String = String::from_utf8_lossy(&raw).replace('\0', " ").trim().to_string();
    if text.chars().count() > 100 {
        let cut: String = text.chars().take(100).collect();
        format!("{cut}...")
    } else {
        text
    }
}

/// Dono do processo: UID de `/proc/<pid>/status` traduzido via `/etc/passwd`.
fn process_user(pid: u32, users: &HashMap<u32, String>) -> String {
    let uid = fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.split_whitespace().next().and_then(|u| u.parse::<u32>().ok()))
        });
    match uid {
        Some(u) => users.get(&u).cloned().unwrap_or_else(|| u.to_string()),
        None => "?".into(),
    }
}

/// Mapa UID -> nome a partir de `/etc/passwd`.
fn load_users() -> HashMap<u32, String> {
    fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let uid = f.nth(1)?.parse().ok()?;
            Some((uid, name.to_string()))
        })
        .collect()
}

/// Formata bytes/s de forma legivel.
fn human_rate(bytes_per_s: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes_per_s >= MB {
        format!("{:.1}MB/s", bytes_per_s / MB)
    } else {
        format!("{:.0}KB/s", bytes_per_s / 1024.0)
    }
}

/// Mede o IO por processo durante `window` e escreve o relatorio no log.
fn report_top_processes(cfg: &Config, logger: &Logger, util: f64) {
    let window = cfg.process_window;
    let before = snapshot_processes();
    let start = Instant::now();
    nap(window);
    let after = snapshot_processes();
    // Usa o tempo real decorrido para calcular taxas corretas.
    let secs = start.elapsed().as_secs_f64().max(0.001);

    let top = top_processes(&before, &after, cfg.top_n);
    if top.is_empty() {
        logger.log(&format!(
            "ALERTA IO={util:.0}% em {} | nenhum processo visivel com IO na janela (IO do kernel/journal?)",
            cfg.device
        ));
        return;
    }

    let users = load_users();
    let list: Vec<String> = top
        .iter()
        .enumerate()
        .map(|(i, u)| {
            format!(
                "{}) {} pid={} user={} R={} W={} cmd=\"{}\"",
                i + 1,
                process_name(u.pid),
                u.pid,
                process_user(u.pid, &users),
                human_rate(u.read as f64 / secs),
                human_rate(u.write as f64 / secs),
                process_cmdline(u.pid),
            )
        })
        .collect();
    logger.log(&format!("ALERTA IO={util:.0}% em {} | {}", cfg.device, list.join(" | ")));
}

// ---------------------------------------------------------------------------
// Linha de comando
// ---------------------------------------------------------------------------

/// Arquivo de configuracao lido quando `-c` nao e informado (se existir).
const DEFAULT_CONFIG: &str = "/etc/iowatchd.conf";

const USAGE: &str = "\
iowatchd - monitora o uso de IO do disco e registra quais processos o consomem

Uso: iowatchd [OPCOES]

Opcoes:
  -c, --config ARQUIVO    arquivo de configuracao (padrao: /etc/iowatchd.conf)
  -d, --device DISCO      disco a monitorar, ex.: sda, nvme0n1 (padrao: auto)
  -t, --threshold PCT     limite de uso de IO em % que dispara o alerta (1-100)
  -i, --interval SEG      segundos entre amostras (minimo 1)
  -n, --top N             quantidade de processos listados por alerta
  -o, --once              faz uma unica medicao, imprime e sai
  -h, --help              mostra esta ajuda
  -V, --version           mostra a versao

As opcoes de linha de comando tem prioridade sobre o arquivo de configuracao.
SIGHUP recarrega a configuracao; SIGTERM/SIGINT encerram o programa.
Codigos de saida: 0 sucesso, 1 erro de execucao, 2 erro de uso.

Exemplos:
  iowatchd --once                 mede o disco da raiz por um intervalo
  iowatchd -d nvme0n1 -t 60       monitora o nvme0n1 com limite de 60%";

/// Opcoes informadas na linha de comando; `None` significa \"nao informado\".
#[derive(Debug, Default, PartialEq)]
struct Cli {
    config: Option<String>,
    device: Option<String>,
    threshold: Option<f64>,
    interval: Option<u64>,
    top_n: Option<usize>,
    once: bool,
}

/// Resultado da leitura dos argumentos.
#[derive(Debug, PartialEq)]
enum Parsed {
    Help,
    Version,
    Run(Cli),
}

impl Cli {
    /// Interpreta os argumentos (sem o nome do programa). Aceita
    /// `--opcao valor`, `--opcao=valor` e `-o valor`. Erros sao textos
    /// prontos para exibir ao usuario (codigo de saida 2).
    fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
        let mut cli = Cli::default();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            // Separa `--opcao=valor` em nome e valor embutido.
            let (name, inline) = match arg.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            };

            // Obtem o valor da opcao: embutido ou o proximo argumento.
            let value = |args: &mut dyn Iterator<Item = String>| -> Result<String, String> {
                inline.clone().or_else(|| args.next()).ok_or_else(|| format!("a opcao '{name}' requer um valor"))
            };
            let bad = |v: &str| format!("valor invalido para '{name}': '{v}'");

            match name.as_str() {
                "-h" | "--help" => return Ok(Parsed::Help),
                "-V" | "--version" => return Ok(Parsed::Version),
                "-o" | "--once" => cli.once = true,
                "-c" | "--config" => cli.config = Some(value(&mut args)?),
                "-d" | "--device" => cli.device = Some(value(&mut args)?),
                "-t" | "--threshold" => {
                    let v = value(&mut args)?;
                    let t: f64 = v.parse().map_err(|_| bad(&v))?;
                    if !(t > 0.0 && t <= 100.0) {
                        return Err(bad(&v));
                    }
                    cli.threshold = Some(t);
                }
                "-i" | "--interval" => {
                    let v = value(&mut args)?;
                    cli.interval = Some(v.parse().ok().filter(|&n| n >= 1).ok_or_else(|| bad(&v))?);
                }
                "-n" | "--top" => {
                    let v = value(&mut args)?;
                    cli.top_n = Some(v.parse().ok().filter(|&n| n >= 1).ok_or_else(|| bad(&v))?);
                }
                other if other.starts_with('-') => return Err(format!("opcao desconhecida '{other}'")),
                other => return Err(format!("argumento inesperado '{other}'")),
            }
        }
        Ok(Parsed::Run(cli))
    }
}

/// Monta a configuracao final: padroes < arquivo < linha de comando,
/// e resolve `device = auto`. Tambem e usada ao recarregar (SIGHUP).
fn build_config(cli: &Cli) -> Result<Config, String> {
    let mut cfg = Config::default();

    // Com -c o arquivo e obrigatorio; sem -c, usa o padrao se ele existir.
    let path = cli.config.as_deref().unwrap_or(DEFAULT_CONFIG);
    match fs::read_to_string(path) {
        Ok(text) => {
            let (parsed, warnings) = Config::parse(&text);
            for w in warnings {
                eprintln!("iowatchd: aviso de configuracao ({path}): {w}");
            }
            cfg = parsed;
        }
        Err(e) if cli.config.is_some() => {
            return Err(format!("nao foi possivel ler {path}: {e}"));
        }
        Err(_) => {}
    }

    if let Some(d) = &cli.device {
        cfg.device = d.clone();
    }
    if let Some(t) = cli.threshold {
        cfg.threshold = t;
    }
    if let Some(i) = cli.interval {
        cfg.interval = i;
    }
    if let Some(n) = cli.top_n {
        cfg.top_n = n;
    }
    cfg.normalize();

    if cfg.device == "auto" {
        cfg.device = detect_device()
            .ok_or("nao foi possivel detectar o disco; use --device ou defina 'device'")?;
    }
    if read_io_ticks(&cfg.device).is_none() {
        return Err(format!("dispositivo '{}' nao encontrado em /proc/diskstats", cfg.device));
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Sinais
// ---------------------------------------------------------------------------

/// Marcados pelo tratador de sinais e consultados no loop principal.
static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    fn sleep(seconds: u32) -> u32;
}

/// Tratador de sinais: so mexe em atomicos, o que e seguro em contexto de sinal.
extern "C" fn on_signal(signum: i32) {
    if signum == 1 {
        RELOAD.store(true, Ordering::SeqCst); // SIGHUP
    } else {
        STOP.store(true, Ordering::SeqCst); // SIGINT / SIGTERM
    }
}

fn install_signal_handlers() {
    // SAFETY: `on_signal` so usa operacoes atomicas, seguras em tratador de sinal.
    unsafe {
        signal(1, on_signal); // SIGHUP
        signal(2, on_signal); // SIGINT
        signal(15, on_signal); // SIGTERM
    }
}

/// Dorme `secs` segundos, mas acorda na hora quando chega um sinal
/// (`std::thread::sleep` voltaria a dormir), permitindo parar e recarregar sem demora.
fn nap(secs: u64) {
    // SAFETY: chamada simples da libc, sem ponteiros.
    unsafe { sleep(secs.min(u32::MAX as u64) as u32) };
}

// ---------------------------------------------------------------------------
// Programa principal
// ---------------------------------------------------------------------------

/// Estado do monitor: IO normal ou em alerta.
enum State {
    Normal { consecutive_high: u32 },
    Alert { last_report: Instant },
}

/// Modo `--once`: mede por um intervalo, imprime e, se acima do limite,
/// lista os processos. Nao escreve em arquivo de log.
fn run_once(cfg: &Config) {
    let logger = Logger { file: None, max_bytes: 0 };
    let start_ticks = read_io_ticks(&cfg.device).unwrap_or(0);
    let start = Instant::now();
    nap(cfg.interval);
    let ticks = read_io_ticks(&cfg.device).unwrap_or(start_ticks);
    let util = utilization(
        ticks.saturating_sub(start_ticks),
        start.elapsed().as_millis() as u64,
    );
    if util >= cfg.threshold {
        report_top_processes(cfg, &logger, util);
    } else {
        println!("IO={util:.0}% em {}", cfg.device);
    }
}

fn make_logger(cfg: &Config) -> Logger {
    Logger { file: cfg.log_file.clone(), max_bytes: cfg.log_max_bytes }
}

fn main() {
    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Help) => return println!("{USAGE}"),
        Ok(Parsed::Version) => return println!("iowatchd {}", env!("CARGO_PKG_VERSION")),
        Ok(Parsed::Run(cli)) => cli,
        Err(e) => {
            eprintln!("iowatchd: {e}\nTente 'iowatchd --help' para mais informacoes.");
            std::process::exit(2);
        }
    };

    let mut cfg = build_config(&cli).unwrap_or_else(|e| {
        eprintln!("iowatchd: erro: {e}");
        std::process::exit(1);
    });

    install_signal_handlers();

    if cli.once {
        run_once(&cfg);
        return;
    }

    let mut logger = make_logger(&cfg);
    let mut prev_ticks = read_io_ticks(&cfg.device).unwrap_or(0);
    let mut prev_time = Instant::now();
    let mut state = State::Normal { consecutive_high: 0 };

    logger.log(&format!(
        "iniciado: device={} limite={}% intervalo={}s",
        cfg.device, cfg.threshold, cfg.interval
    ));

    loop {
        nap(cfg.interval);

        if STOP.load(Ordering::SeqCst) {
            logger.log("encerrado");
            break;
        }

        if RELOAD.swap(false, Ordering::SeqCst) {
            match build_config(&cli) {
                Ok(new) => {
                    cfg = new;
                    logger = make_logger(&cfg);
                    // O disco pode ter mudado: reinicia as referencias e o estado.
                    prev_ticks = read_io_ticks(&cfg.device).unwrap_or(0);
                    prev_time = Instant::now();
                    state = State::Normal { consecutive_high: 0 };
                    logger.log(&format!(
                        "configuracao recarregada: device={} limite={}% intervalo={}s",
                        cfg.device, cfg.threshold, cfg.interval
                    ));
                }
                Err(e) => logger.log(&format!("falha ao recarregar ({e}); mantendo a configuracao atual")),
            }
            continue;
        }

        let Some(ticks) = read_io_ticks(&cfg.device) else {
            eprintln!("aviso: falha ao ler /proc/diskstats");
            continue;
        };
        let now = Instant::now();
        let util = utilization(
            ticks.saturating_sub(prev_ticks),
            now.duration_since(prev_time).as_millis() as u64,
        );
        prev_ticks = ticks;
        prev_time = now;

        state = match state {
            State::Normal { consecutive_high } => {
                if util >= cfg.threshold {
                    let count = consecutive_high + 1;
                    if count >= cfg.trigger_samples {
                        report_top_processes(&cfg, &logger, util);
                        // A medicao de processos consumiu tempo; reinicia a
                        // referencia para a proxima amostra do disco.
                        prev_ticks = read_io_ticks(&cfg.device).unwrap_or(prev_ticks);
                        prev_time = Instant::now();
                        State::Alert { last_report: Instant::now() }
                    } else {
                        State::Normal { consecutive_high: count }
                    }
                } else {
                    State::Normal { consecutive_high: 0 }
                }
            }
            State::Alert { last_report } => {
                if util < cfg.clear_threshold {
                    logger.log(&format!("IO normalizado: {util:.0}% em {}", cfg.device));
                    State::Normal { consecutive_high: 0 }
                } else if util >= cfg.threshold
                    && last_report.elapsed() >= Duration::from_secs(cfg.cooldown)
                {
                    report_top_processes(&cfg, &logger, util);
                    prev_ticks = read_io_ticks(&cfg.device).unwrap_or(prev_ticks);
                    prev_time = Instant::now();
                    State::Alert { last_report: Instant::now() }
                } else {
                    State::Alert { last_report }
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_ticks_e_lido_do_dispositivo_certo() {
        let s = "   8       0 sda 100 0 800 50 200 0 1600 70 0 4242 120 0 0 0 0 0 0\n\
                 \x20  8       1 sda1 1 0 8 5 2 0 16 7 0 99 12 0 0 0 0 0 0\n";
        assert_eq!(parse_io_ticks(s, "sda"), Some(4242));
        assert_eq!(parse_io_ticks(s, "sda1"), Some(99));
        assert_eq!(parse_io_ticks(s, "nvme0n1"), None);
    }

    #[test]
    fn cli_aceita_formas_longas_curtas_e_igual() {
        let a = |v: &[&str]| Cli::parse(v.iter().map(|s| s.to_string()));
        assert_eq!(a(&["-h"]), Ok(Parsed::Help));
        assert_eq!(a(&["--version"]), Ok(Parsed::Version));
        let Ok(Parsed::Run(c)) = a(&["-d", "nvme0n1", "--threshold=60", "-i", "5", "--once"]) else {
            panic!("falhou")
        };
        assert_eq!(c.device.as_deref(), Some("nvme0n1"));
        assert_eq!(c.threshold, Some(60.0));
        assert_eq!(c.interval, Some(5));
        assert!(c.once);
    }

    #[test]
    fn cli_rejeita_entradas_invalidas() {
        let a = |v: &[&str]| Cli::parse(v.iter().map(|s| s.to_string()));
        assert!(a(&["--bogus"]).is_err());
        assert!(a(&["arquivo"]).is_err());
        assert!(a(&["-t"]).is_err());
        assert!(a(&["-t", "0"]).is_err());
        assert!(a(&["-t", "101"]).is_err());
        assert!(a(&["-i", "0"]).is_err());
        assert!(a(&["-n", "abc"]).is_err());
    }

    #[test]
    fn origem_da_raiz_e_extraida_do_mountinfo() {
        let m = "22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n\
                 30 22 0:25 / /tmp rw - tmpfs tmpfs rw\n\
                 40 22 0:40 /@ / rw - btrfs /dev/mapper/cryptroot rw\n";
        assert_eq!(parse_root_source(m).as_deref(), Some("/dev/mapper/cryptroot"));
        assert_eq!(parse_root_source("1 0 0:1 / / rw - overlay overlay rw\n"), None);
    }

    #[test]
    fn utilizacao_e_limitada_a_100() {
        assert_eq!(utilization(500, 1000), 50.0);
        assert_eq!(utilization(2000, 1000), 100.0);
        assert_eq!(utilization(10, 0), 0.0);
    }

    #[test]
    fn proc_io_le_bytes_reais() {
        let t = "rchar: 10\nwchar: 20\nread_bytes: 4096\nwrite_bytes: 8192\n";
        assert_eq!(parse_proc_io(t), Some(ProcIo { read: 4096, write: 8192 }));
        assert_eq!(parse_proc_io("rchar: 1\n"), None);
    }

    #[test]
    fn top_ordena_e_ignora_processos_sem_io() {
        let before: HashMap<u32, ProcIo> = [(1, ProcIo { read: 0, write: 0 })].into();
        let after: HashMap<u32, ProcIo> = [
            (1, ProcIo { read: 0, write: 0 }),
            (2, ProcIo { read: 100, write: 0 }),
            (3, ProcIo { read: 0, write: 900 }),
        ]
        .into();
        let top = top_processes(&before, &after, 5);
        assert_eq!(top.iter().map(|u| u.pid).collect::<Vec<_>>(), vec![3, 2]);
        assert_eq!(top_processes(&before, &after, 1).len(), 1);
    }

    #[test]
    fn config_aplica_valores_e_avisa_sobre_erros() {
        let (c, w) = Config::parse("device = nvme0n1\nthreshold=90 # x\ninterval = abc\nfoo = 1\nlog_file =\n");
        assert_eq!(c.device, "nvme0n1");
        assert_eq!(c.threshold, 90.0);
        assert_eq!(c.interval, 2);
        assert!(c.log_file.is_none());
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn config_corrige_histerese_invertida() {
        let (c, _) = Config::parse("threshold = 50\nclear_threshold = 70\ninterval = 0\n");
        assert_eq!(c.clear_threshold, 50.0);
        assert_eq!(c.interval, 1);
    }
}
