import Foundation

extension DJStatus {
    private enum StatusKeys: String, CodingKey { case zone, running, now, next, steering, problem }

    /// DjStatusDto omits an empty `next` array. In particular a stopped DJ
    /// normally has neither `now` nor `next`; it must still be readable before
    /// the owner can start or steer it. A present malformed queue is an error,
    /// not permission to substitute an empty queue.
    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: StatusKeys.self)
        self.init(
            zone: try values.decode(String.self, forKey: .zone),
            running: try values.decode(Bool.self, forKey: .running),
            now: try values.decodeIfPresent(DJWork.self, forKey: .now),
            next: try values.contains(.next) ? values.decode([DJWork].self, forKey: .next) : [],
            steering: try values.decode(Steering.self, forKey: .steering),
            problem: try values.decodeIfPresent(String.self, forKey: .problem)
        )
    }
}
