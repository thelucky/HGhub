export type Author = { id: number; name: string; avatar?: string; followers?: number };

export type VideoCard = {
  id: number;
  title: string;
  cover: string;
  description?: string;
  tags: string[];
  score: number;
  episode_count: number;
  total_episodes: number;
  duration?: string;
  hot: number;
  like_count: number;
  collect_count: number;
  comment_count: number;
  is_finished: boolean;
  is_original: number;
  created_at?: string;
  last_ep_at?: number;
  new_at?: number;
  author?: Author;
};

export type Pagination = { page: number; pages: number; size: number; total: number };
export type Paged<T> = { items: T[]; pagination: Pagination };

export type Episode = { ep_num: number; title: string; duration: number; status: number };

export type Detail = {
  id: number;
  title: string;
  description: string;
  cover: string;
  tags: string[];
  tag_links?: { name: string; url: string }[];
  score: number;
  episode_count: number;
  total_episodes: number;
  episodes: Episode[];
  counts: { collect: number; comment: number; hot: number; like: number };
  author: Author & { is_following?: boolean; following?: number; like_count?: number };
  interaction?: { collected: boolean; liked: boolean; liked_ep?: number };
  video_url?: string;
  video_url_hevc?: string;
  is_finished: boolean;
  is_original: number;
  video_type?: number;
  duration?: number;
  created_at?: string;
};

export type PlayInfo = {
  id: number;
  ep_num: number;
  duration?: number;
  title?: string;
  video_url: string;
  video_url_hevc?: string;
};

export type RankItem = {
  rank: number;
  video_id: number;
  title: string;
  cover: string;
  tags: string[];
  score: number;
  cate_name: string;
  episode_count: number;
  is_finished: boolean;
  metric_label: string;
  metric_value: number;
  metrics: { collect: number; comment: number; like: number; play: number };
};

export type Comment = {
  id: number;
  user_id: number;
  nickname: string;
  avatar: string;
  avatar_color: string;
  text: string;
  time: string;
  created_at?: string;
  like_count: number;
  liked: boolean;
  reply_count: number;
  reply_id: number;
  reply_list?: Comment[];
  is_pinned?: boolean;
  ip_region?: string;
};

export type CommentPage = {
  ep: number;
  revision?: string;
  reset?: boolean;
  items: Comment[];
  pagination: Pagination;
};

export type Post = {
  id: number;
  title: string;
  desc?: string;
  content?: string;
  cover: string;
  tags: string[];
  hot: number;
  video_url?: string;
  video_id?: number;
  author?: { id: number; name: string; avatar?: string; is_following?: boolean };
  counts?: { like: number; comment: number; collect: number; share?: number; hot?: number };
  created_at?: string;
  publish_time?: string;
};

export type User = {
  id: number;
  username: string;
  nickname: string;
  avatar: string;
  avatar_color: string;
  sign?: string;
  desc?: string;
  user_type?: string;
  counts?: Record<string, number>;
  interactions?: {
    like_video_ids: number[];
    favorite_video_ids: number[];
    follow_user_ids: number[];
    like_video_episodes: { video_id: number; ep: number }[];
  };
};
