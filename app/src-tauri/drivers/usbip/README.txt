usbip-win2 安装包（随应用捆绑，供「一键安装」使用）
====================================================

文件
----
  USBip-0.9.8.0-x64.exe   官方 Inno Setup 安装包，26,390,744 字节

来源
----
  https://github.com/vadimgrn/usbip-win2/releases/tag/v.0.9.8.0
  https://github.com/vadimgrn/usbip-win2/releases/download/v.0.9.8.0/USBip-0.9.8.0-x64.exe
  SHA256（用于校验，请勿手工修改本目录内的 exe）：

    81F426741F7EE2ED991FEBE24A22DACA8400B6AE2F171054E3FB404897E15D39

  安装包本体 Authenticode 签名有效，签署者：
    CN=Cloudyne Systems (Scheibling Consulting AB) —— Open Source Codesigning
    Initiative (OSSign) 提供的开源项目代码签名证书。

安装包会做什么
--------------
  * 释放到 %ProgramFiles%\USBip\：usbip.exe（CLI）、devnode.exe、wusbip.exe（GUI）、
    usbip2_ude.sys（UDE 虚拟主控驱动）、usbip2_filter.sys（设备上层过滤驱动）；
  * `pnputil /add-driver usbip2_filter.inf /install` 安装过滤驱动；
  * `devnode.exe install usbip2_ude.inf ROOT\USBIP_WIN2\UDE` 创建虚拟主控设备；
  * 注册计划任务「USBip Detach All On Reboot Or Shutdown」（关机/重启前自动 detach）。

  安装过程中会重启 USB 3.0 集线器（所有 USB 设备短暂中断），安装程序也会要求重启。
  需要管理员权限（UAC）。

本应用如何使用
--------------
  * `usbip install`（UI: 安装 USB/IP 驱动）→ 以管理员身份静默运行本安装包：
        USBip-0.9.8.0-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-
  * `usbip attach -r 127.0.0.1 -b 1-N` → 把内置服务器仿真的虚拟线缆接入系统；
  * `usbip port` / `usbip detach -p N` → 查看/断开已接入的线缆。

  仅支持 x64；Windows 10 1903 (build 18362) 及以上。

许可
----
  usbip-win2 为 BSD 2-Clause（Copyright (c) 2021-2026 Vadym Hrynchyshyn），
  详见同目录 LICENSE.txt。以二进制形式再分发需保留该版权声明与免责声明。
