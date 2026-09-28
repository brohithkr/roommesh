import CoreAudio
import CryptoKit
import Foundation

enum VirtualDeviceStatus {
    static let micUID = "RoomMeshMicrophone_UID"
    static let speakerUID = "RoomMeshSpeaker_UID"
    static let installedDriverURL = URL(fileURLWithPath: "/Library/Audio/Plug-Ins/HAL/RoomMesh.driver")

    static func deviceExists(uid: String) -> Bool {
        var address = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyTranslateUIDToDevice,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        var cfUID: CFString = uid as CFString
        var device = AudioObjectID(kAudioObjectUnknown)
        var size = UInt32(MemoryLayout<AudioObjectID>.size)
        let status = withUnsafeMutablePointer(to: &cfUID) { q in
            AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address,
                                       UInt32(MemoryLayout<CFString>.size), q, &size, &device)
        }
        return status == noErr && device != kAudioObjectUnknown
    }
    static var installed: Bool { deviceExists(uid: micUID) && deviceExists(uid: speakerUID) }
    static var installedVersion: String? { version(ofDriver: installedDriverURL) }

    /// Reads `Contents/Info.plist` directly: `Bundle(url:)` caches the instance and its
    /// infoDictionary, so it would keep reporting the version from before an update.
    static func version(ofDriver driver: URL) -> String? {
        NSDictionary(contentsOf: driver.appendingPathComponent("Contents/Info.plist"))?["CFBundleShortVersionString"] as? String
    }
    static func executableURL(inDriver driver: URL) -> URL { driver.appendingPathComponent("Contents/MacOS/RoomMesh") }

    /// SHA-256 (hex) of a file, or nil if it can't be read. Memoized per path on (modification date, size),
    /// so the 2 s status poll doesn't rehash an unchanged ~1 MB executable.
    static func executableHash(_ url: URL) -> String? { hashCache.hash(of: url) }
    private static let hashCache = FileHashCache()
}

private final class FileHashCache: @unchecked Sendable {
    private struct Entry { let modified: Date; let size: Int; let hash: String }
    private let lock = NSLock()
    private var entries: [String: Entry] = [:]

    func hash(of url: URL) -> String? {
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: url.path),
              let modified = attrs[.modificationDate] as? Date, let size = (attrs[.size] as? NSNumber)?.intValue else {
            return nil
        }
        lock.lock(); let cached = entries[url.path]; lock.unlock()
        if let cached, cached.modified == modified, cached.size == size { return cached.hash }
        guard let data = try? Data(contentsOf: url, options: .mappedIfSafe) else { return nil }
        let hex = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        lock.lock(); entries[url.path] = Entry(modified: modified, size: size, hash: hex); lock.unlock()
        return hex
    }
}
