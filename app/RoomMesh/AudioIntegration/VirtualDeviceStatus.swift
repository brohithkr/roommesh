import CoreAudio
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
    static var installedVersion: String? {
        Bundle(url: installedDriverURL)?.infoDictionary?["CFBundleShortVersionString"] as? String
    }
}
