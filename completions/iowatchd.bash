# bash completion for iowatchd and iowatchd-log

_iowatchd() {
    local cur prev
    cur=${COMP_WORDS[COMP_CWORD]}
    prev=${COMP_WORDS[COMP_CWORD-1]}
    case $prev in
        -c|--config) COMPREPLY=($(compgen -f -- "$cur")); return ;;
        -d|--device)
            COMPREPLY=($(compgen -W "auto $(awk '{print $3}' /proc/diskstats 2>/dev/null)" -- "$cur"))
            return ;;
        -t|--threshold|-i|--interval|-n|--top) return ;;
    esac
    COMPREPLY=($(compgen -W "-c --config -d --device -t --threshold -i --interval
        -n --top -j --json -o --once -C --check-config -L --list-devices
        -f --foreground -q --quiet -v --verbose -h --help -V --version" -- "$cur"))
}
complete -F _iowatchd iowatchd

_iowatchd_log() {
    local cur prev
    cur=${COMP_WORDS[COMP_CWORD]}
    prev=${COMP_WORDS[COMP_CWORD-1]}
    case $prev in
        -n|--lines|--since) return ;;
    esac
    COMPREPLY=($(compgen -W "-n --lines -f --follow --since -j --json -s --status
        --no-pager -h --help" -- "$cur"))
}
complete -F _iowatchd_log iowatchd-log
