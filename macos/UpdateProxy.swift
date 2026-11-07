import Foundation
import CFNetwork

@_cdecl("clip_update_system_proxy")
public func clipUpdateSystemProxy(
    _ target: UnsafePointer<CChar>, _ buffer: UnsafeMutablePointer<CChar>, _ capacity: Int
) -> Bool {
    guard let url = URL(string: String(cString: target)),
          let settings = CFNetworkCopySystemProxySettings()?.takeRetainedValue(),
          let proxies = CFNetworkCopyProxiesForURL(url as CFURL, settings).takeRetainedValue() as? [[String: Any]],
          let proxy = proxies.first,
          let type = proxy[kCFProxyTypeKey as String] as? String,
          let host = proxy[kCFProxyHostNameKey as String] as? String,
          let port = proxy[kCFProxyPortNumberKey as String] as? Int,
          (1...65535).contains(port) else { return false }

    var endpoint = URLComponents()
    if type == (kCFProxyTypeHTTP as String) || type == (kCFProxyTypeHTTPS as String) {
        // macOS's HTTPS proxy setting describes HTTP CONNECT for HTTPS destinations.
        endpoint.scheme = "http"
    } else if type == (kCFProxyTypeSOCKS as String) {
        endpoint.scheme = "socks5h"
    } else {
        return false
    }
    endpoint.host = host.contains(":") && !host.hasPrefix("[") ? "[\(host)]" : host
    endpoint.port = port
    guard let address = endpoint.url?.absoluteString else { return false }
    let bytes = address.utf8CString
    guard bytes.count <= capacity else { return false }
    for (index, byte) in bytes.enumerated() { buffer[index] = byte }
    return true
}
