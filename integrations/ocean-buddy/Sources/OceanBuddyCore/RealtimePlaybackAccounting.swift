import Foundation

/// Pure accounting for the AVAudioPlayerNode buffer queue. Queue depth and
/// playback credit advance only from player completion callbacks, never from
/// elapsed wall time. Partial buffers are intentionally not credited, so a
/// barge-in truncation can understate but never overstate rendered audio.
struct BuddyPlaybackQueueAccounting: Sendable {
    struct Reservation: Equatable, Sendable {
        let id: UInt64
        let queueEpoch: UInt64
        let responseID: UInt64
        let frames: UInt64
    }

    static let sampleRate: UInt64 = 24_000
    static let maximumQueuedFrames: UInt64 = 5 * sampleRate

    private var queueEpoch: UInt64 = 0
    private var nextReservationID: UInt64 = 0
    private var nextResponseID: UInt64 = 0
    private var activeResponseID: UInt64?
    private var activeResponseEnded = false
    private var queue: [Reservation] = []
    private var queuedFrames: UInt64 = 0
    private var completedActiveResponseFrames: UInt64 = 0

    /// Starts accounting for a new assistant item without discarding buffers
    /// already scheduled for an earlier item.
    mutating func beginResponse() {
        nextResponseID &+= 1
        activeResponseID = nextResponseID
        activeResponseEnded = false
        completedActiveResponseFrames = 0
    }

    mutating func reserve(frames: UInt64) -> Reservation? {
        guard frames > 0,
              frames <= Self.maximumQueuedFrames,
              queuedFrames <= Self.maximumQueuedFrames - frames
        else {
            return nil
        }
        if activeResponseID == nil {
            beginResponse()
        }
        guard let activeResponseID else { return nil }

        nextReservationID &+= 1
        let reservation = Reservation(
            id: nextReservationID,
            queueEpoch: queueEpoch,
            responseID: activeResponseID,
            frames: frames
        )
        queue.append(reservation)
        queuedFrames += frames
        return reservation
    }

    /// Records frames only after AVAudioPlayerNode reports that the entire
    /// scheduled buffer played back. Stale callbacks from a reset queue are
    /// ignored by epoch.
    mutating func didPlay(_ reservation: Reservation) {
        guard reservation.queueEpoch == queueEpoch,
              let index = queue.firstIndex(where: { $0.id == reservation.id })
        else {
            return
        }
        queuedFrames -= reservation.frames
        queue.remove(at: index)
        if reservation.responseID == activeResponseID {
            completedActiveResponseFrames += reservation.frames
            if activeResponseEnded,
               !queue.contains(where: { $0.responseID == reservation.responseID }) {
                activeResponseID = nil
                activeResponseEnded = false
                completedActiveResponseFrames = 0
            }
        }
    }

    /// Mark provider generation complete while retaining item identity and
    /// completion-backed playback credit until its scheduled audio drains. A
    /// user can still interrupt unheard queued audio after `response.done`.
    @discardableResult
    mutating func endResponse() -> Bool {
        guard let activeResponseID else { return false }
        activeResponseEnded = true
        let hasQueuedActiveOutput = queue.contains {
            $0.responseID == activeResponseID
        }
        if !hasQueuedActiveOutput {
            self.activeResponseID = nil
            activeResponseEnded = false
            completedActiveResponseFrames = 0
        }
        return hasQueuedActiveOutput
    }

    /// Returns conservative, completion-backed playback for the active item and
    /// invalidates every scheduled reservation. No wall-time estimate is added.
    mutating func interrupt() -> UInt64 {
        let playedFrames = completedActiveResponseFrames
        _ = reset()
        return playedFrames
    }

    @discardableResult
    mutating func reset() -> Bool {
        let hadQueuedOutput = !queue.isEmpty
        queueEpoch &+= 1
        queue.removeAll(keepingCapacity: true)
        queuedFrames = 0
        activeResponseID = nil
        activeResponseEnded = false
        completedActiveResponseFrames = 0
        return hadQueuedOutput
    }
}
