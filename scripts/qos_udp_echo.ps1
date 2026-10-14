# ── QOS UDP 回声器（Windows PowerShell 端，监听 UDP 15390）──
$ErrorActionPreference = "SilentlyContinue"
$u = New-Object System.Net.Sockets.UdpClient(15390)
$u.Client.ReceiveBufferSize = 1048576
$u.Client.SendBufferSize = 1048576
$ep = New-Object System.Net.IPEndPoint([System.Net.IPAddress]::Any, 0)
while ($true) {
    try {
        $d = $u.Receive([ref]$ep)
        if ($d.Length -ge 8) {
            $prefix = [System.Text.Encoding]::ASCII.GetString($d, 0, 8)
            if ($prefix -eq "QOSPROBE" -or $prefix -eq "QOSBURST") {
                [void]$u.Send($d, $d.Length, $ep)
            }
        }
    } catch {
    }
}
