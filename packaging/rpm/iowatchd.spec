%global debug_package %{nil}

Name:           iowatchd
Version:        0.4.2
Release:        1%{?dist}
Summary:        Lightweight disk IO monitor that logs which processes cause high IO
License:        MIT
URL:            https://github.com/Thiapalm/iowatchd
Source0:        %{url}/archive/refs/tags/v%{version}.tar.gz#/%{name}-%{version}.tar.gz
BuildRequires:  cargo
BuildRequires:  make
BuildRequires:  systemd-rpm-macros
%{?systemd_requires}

%description
iowatchd samples /proc/diskstats with minimal resources and, when disk
utilization exceeds a threshold, logs the processes doing the most IO to
journald and a log file.

%prep
%autosetup

%build
make

%install
make install DESTDIR=%{buildroot} PREFIX=%{_prefix} \
    SYSTEMDUNITDIR=%{_unitdir} LICENSEDIR=%{_licensedir}/%{name}

%post
%systemd_post iowatchd.service

%preun
%systemd_preun iowatchd.service

%postun
%systemd_postun_with_restart iowatchd.service

%files
%license LICENSE
%config(noreplace) %{_sysconfdir}/iowatchd.conf
%config(noreplace) %{_sysconfdir}/logrotate.d/iowatchd
%{_bindir}/iowatchd
%{_bindir}/iowatchd-log
%{_bindir}/iowatchd-notify
%{_unitdir}/iowatchd.service
%{_mandir}/man1/iowatchd-log.1*
%{_mandir}/man5/iowatchd.conf.5*
%{_mandir}/man8/iowatchd.8*
%{_datadir}/bash-completion/completions/iowatchd
%{_datadir}/zsh/site-functions/_iowatchd
%{_datadir}/fish/vendor_completions.d/iowatchd.fish

%changelog
* Sat Oct 17 2026 Thiapalm <thiago.palmieri@gmail.com> - 0.4.2-1
- Update to 0.4.2
* Sat Oct 17 2026 Thiapalm <thiago.palmieri@gmail.com> - 0.4.1-1
- Update to 0.4.1
* Sat Oct 17 2026 Thiapalm <thiago.palmieri@gmail.com> - 0.4.0-1
- Update to 0.4.0
* Sat Oct 17 2026 Thiapalm <thiago.palmieri@gmail.com> - 0.3.1-1
- Update to 0.3.1
* Sat Oct 17 2026 Thiapalm <thiago.palmieri@gmail.com> - 0.3.0-1
- Update to 0.3.0
* Sat Oct 17 2026 Thiapalm <thiago.palmieri@gmail.com> - 0.2.0-1
- Initial package
